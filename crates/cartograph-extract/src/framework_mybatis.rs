//! `MyBatis` `SqlSessionTemplate` statement-id references (v1 `mybatis-template-binding`).
//!
//! `getSqlSessionTemplate().delete(SQL_NS + ".deleteByOrderId", id)` with
//! `String SQL_NS = OrderAttributeDao.class.getName() + "Mapper"` names the XML
//! statement `OrderAttributeDaoMapper::deleteByOrderId`. The first argument is
//! evaluated from string literals, same-file class-level `String` fields the
//! walker extracted and that are assigned exactly once (v1
//! `parseStringConstantDeclaration`), and
//! `X.class.getName()` / `getCanonicalName()` (the simple class name, matching
//! the XML extractor's simple-namespace statement names). The resulting
//! `<Mapper>::<statement>` reference is owned by the enclosing method, and the
//! existing Java-to-XML project resolution binds it to the XML statement.
//! Anything that cannot be evaluated exactly (method calls, locals, shadowed or
//! reassigned names, declarations or calls spelled inside literals, and any
//! file containing a `\uXXXX` Unicode escape or an identifier-ignorable
//! character in code) emits nothing. All structure is read from a copy of the
//! source whose literal contents are blanked, so literal text never becomes a
//! declaration, an assignment, a binding, or a call.

use std::{cmp::Reverse, collections::HashMap};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};

use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkReferenceInput, skip_ascii_whitespace},
};

/// Receivers of template calls; `getSqlSessionTemplate` must be invoked as `()`.
const TEMPLATE_RECEIVERS: &[(&str, bool)] = &[
    ("getSqlSessionTemplate", true),
    ("sqlSessionTemplate", false),
];
const TEMPLATE_METHODS: &[&str] = &[
    "selectOne",
    "selectList",
    "selectMap",
    "insert",
    "update",
    "delete",
];
const CLASS_NAME_SUFFIXES: &[&str] = &[".class.getName()", ".class.getCanonicalName()"];
/// Same-file `String` constants considered; larger files keep the first ones.
const MAX_STRING_CONSTANTS: usize = 256;
/// Longest evaluated statement id or constant value.
const MAX_VALUE_BYTES: usize = 512;
/// Longest first-argument expression scanned.
const MAX_ARGUMENT_BYTES: usize = 2_048;
/// Longest single-line constant declaration considered.
const MAX_DECLARATION_BYTES: usize = 1_024;
/// Largest method whose own bindings are scanned; a larger caller abstains.
const MAX_SCOPE_BYTES: usize = 64 * 1_024;
/// Byte visits for a method scan, including its lambda parameter tokens.
/// Overlapping or malformed parameter lists exhaust this budget and abstain.
const MAX_SCOPE_WORK: usize = 2 * MAX_SCOPE_BYTES;
/// Template calls evaluated per file.
const MAX_TEMPLATE_CALLS: usize = 4_096;
/// Lines, identifiers, or bytes scanned between cancellation polls.
const CANCELLATION_POLL_INTERVAL: usize = 1_024;
const TEXT_BLOCK_QUOTE: &[u8] = b"\"\"\"";
/// Hex digits of a Java `\uXXXX` escape.
const UNICODE_ESCAPE_DIGITS: usize = 4;

/// The comment-masked source and the same text with literal contents blanked.
/// Both have identical byte offsets: structure is read from `code`, evaluated
/// expressions are sliced from `source`.
#[derive(Clone, Copy)]
struct JavaText<'source> {
    source: &'source str,
    code: &'source str,
}

struct StringConstant<'source> {
    name: &'source str,
    expression: &'source str,
}

/// Evaluated same-file `String` constants, in declaration order.
struct Constants<'source> {
    values: Vec<(&'source str, String)>,
}

impl Constants<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| value.as_str())
    }
}

/// One template call's first argument.
struct TemplateArgument<'source> {
    expression: &'source str,
    start: usize,
    call: usize,
}

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Java || !source.contains("qlSessionTemplate") {
        return Ok(());
    }
    if has_unicode_escape(builder)? {
        return Ok(());
    }
    let code = mask_literals(builder, source)?;
    if has_ignorable_code_char(builder, &code)? {
        return Ok(());
    }
    let text = JavaText {
        source,
        code: &code,
    };
    let constants = evaluate_constants(builder, text)?;
    let arguments = template_arguments(builder, text)?;
    let mut callables = CallableIndex::new(builder);
    let mut scopes = MethodBindings::new(&code);
    for argument in arguments {
        builder.check_cancelled()?;
        let Some(method) = callables.innermost(argument.call) else {
            continue;
        };
        // A parameter, local, or lambda binding of the calling method shadows
        // a same-named field constant.
        let bound = scopes.bindings(method, &mut || builder.check_cancelled())?;
        if shadows(bound, argument.expression) {
            continue;
        }
        let owner = method.id.clone();
        let Some(statement) = evaluate_expression(argument.expression, &constants)
            .as_deref()
            .filter(|value| !crate::walk::specifier_safety::specifier_may_carry_credential(value))
            .and_then(statement_qualified_name)
        else {
            continue;
        };
        builder.add_reference(FrameworkReferenceInput {
            owner: Some(owner),
            name: &statement,
            resolution_name: None,
            kind: ReferenceKind::References,
            start: argument.start,
            end: argument.start + argument.expression.len(),
        })?;
    }
    Ok(())
}

/// Whether the raw (not comment-masked) file holds a Java Unicode escape
/// (`\uXXXX`, any number of `u`s). Java translates these before it finds
/// comments, literals, or identifiers, so an escape can end a comment, spell
/// a quote or text-block delimiter, or respell an identifier (including with
/// an identifier-ignorable character) in ways the raw-text scans here cannot
/// see; the whole file abstains rather than guess.
fn has_unicode_escape(builder: &mut FrameworkBuilder<'_, '_>) -> Result<bool, ExtractError> {
    let source = builder.source();
    for (index, (start, _)) in source.match_indices("\\u").enumerate() {
        if index.is_multiple_of(CANCELLATION_POLL_INTERVAL) {
            builder.check_cancelled()?;
        }
        let digits = source[start + 1..].trim_start_matches('u');
        let escape = digits
            .get(..UNICODE_ESCAPE_DIGITS)
            .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if escape {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether literal- and comment-masked `code` holds a character Java ignores
/// inside identifiers (`Character.isIdentifierIgnorable`: the listed control
/// characters and format characters such as U+200B). `N\u{200B}S` is the
/// identifier `NS` to Java but a different spelling to the scans here, so
/// such a file abstains.
fn has_ignorable_code_char(
    builder: &mut FrameworkBuilder<'_, '_>,
    code: &str,
) -> Result<bool, ExtractError> {
    for (index, character) in code.chars().enumerate() {
        if index.is_multiple_of(CANCELLATION_POLL_INTERVAL) {
            builder.check_cancelled()?;
        }
        if IDENTIFIER_IGNORABLE
            .iter()
            .any(|range| range.contains(&character))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Java identifier-ignorable characters: C0/C1 controls that are not
/// whitespace, plus the Unicode format (`Cf`) characters.
const IDENTIFIER_IGNORABLE: &[std::ops::RangeInclusive<char>] = &[
    '\u{0}'..='\u{8}',
    '\u{e}'..='\u{1b}',
    '\u{7f}'..='\u{9f}',
    '\u{ad}'..='\u{ad}',
    '\u{600}'..='\u{605}',
    '\u{61c}'..='\u{61c}',
    '\u{6dd}'..='\u{6dd}',
    '\u{70f}'..='\u{70f}',
    '\u{890}'..='\u{891}',
    '\u{8e2}'..='\u{8e2}',
    '\u{180e}'..='\u{180e}',
    '\u{200b}'..='\u{200f}',
    '\u{202a}'..='\u{202e}',
    '\u{2060}'..='\u{2064}',
    '\u{2066}'..='\u{206f}',
    '\u{feff}'..='\u{feff}',
    '\u{fff9}'..='\u{fffb}',
    '\u{110bd}'..='\u{110bd}',
    '\u{110cd}'..='\u{110cd}',
    '\u{13430}'..='\u{1343f}',
    '\u{1bca0}'..='\u{1bca3}',
    '\u{1d173}'..='\u{1d17a}',
    '\u{e0001}'..='\u{e0001}',
    '\u{e0020}'..='\u{e007f}',
];

/// A copy of `source` whose string, char, and text-block contents are spaces
/// (delimiters and newlines kept), in one cancellable forward pass. Every
/// replaced byte becomes ASCII, so the copy stays UTF-8 with the same offsets.
fn mask_literals(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<String, ExtractError> {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut tracker = LiteralTracker::default();
    let mut index = 0;
    let mut steps = 0_usize;
    while index < bytes.len() {
        steps += 1;
        if steps.is_multiple_of(CANCELLATION_POLL_INTERVAL) {
            builder.check_cancelled()?;
        }
        let inside = tracker.state != LiteralState::Code;
        let width = tracker.advance(bytes, index);
        if inside && tracker.state != LiteralState::Code {
            for byte in &mut masked[index..index + width] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
        index += width;
    }
    String::from_utf8(masked).map_err(|_| ExtractError::InvalidSpan)
}

fn evaluate_constants<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    text: JavaText<'source>,
) -> Result<Constants<'source>, ExtractError> {
    let mut callables = CallableIndex::new(builder);
    let mut offset = 0_usize;
    let mut declarations = Vec::new();
    for (index, line) in text.code.split_inclusive('\n').enumerate() {
        if index.is_multiple_of(CANCELLATION_POLL_INTERVAL) {
            builder.check_cancelled()?;
        }
        let line_start = offset;
        offset += line.len();
        if declarations.len() >= MAX_STRING_CONSTANTS || line.len() > MAX_DECLARATION_BYTES {
            continue;
        }
        // Only class-level fields are constants; a method local is scoped to
        // its method and must never answer for another method's identifier.
        if let Some(declaration) = string_constant(text, line_start, line)
            && callables.innermost(declaration.equals_offset).is_none()
        {
            declarations.push(declaration);
        }
    }
    // An initializer-block local is outside every callable too, so the
    // walker must also have extracted a field of that name at that `=`.
    let fields = FieldSpans::new(builder);
    let mut declarations = declarations
        .into_iter()
        .filter(|declaration| fields.declares(declaration))
        .map(|declaration| declaration.constant)
        .collect::<Vec<_>>();
    // A name declared twice (shadowing) or assigned again anywhere in the file
    // has no single provable value, so it is not evaluated at all.
    let assignments = assignment_counts(builder, text.code, &declarations)?;
    declarations.retain(|declaration| {
        assignments
            .iter()
            .find(|(name, _)| *name == declaration.name)
            .is_some_and(|(_, count)| *count == 1)
    });
    resolve_constants(builder, &declarations)
}

/// Resolve the validated fields in declaration order to a bounded fixed point.
fn resolve_constants<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    declarations: &[StringConstant<'source>],
) -> Result<Constants<'source>, ExtractError> {
    let mut constants = Constants { values: Vec::new() };
    // Fixed point: each pass resolves at least one more constant or stops, so
    // the loop runs at most `declarations.len()` times.
    for _ in 0..declarations.len() {
        builder.check_cancelled()?;
        let mut progressed = false;
        for declaration in declarations {
            if constants.get(declaration.name).is_some() {
                continue;
            }
            if let Some(value) = evaluate_expression(declaration.expression, &constants) {
                constants.values.push((declaration.name, value));
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    Ok(constants)
}

/// How often each declared constant name is the target of `=` or `+=` in the
/// literal-masked code (its own declaration counts once), in one bounded pass.
fn assignment_counts<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    code: &str,
    declarations: &[StringConstant<'source>],
) -> Result<Vec<(&'source str, usize)>, ExtractError> {
    let mut counts: Vec<(&str, usize)> = declarations
        .iter()
        .map(|declaration| (declaration.name, 0_usize))
        .collect::<Vec<_>>();
    if counts.is_empty() {
        return Ok(counts);
    }
    let bytes = code.as_bytes();
    let mut cursor = 0;
    let mut steps = 0_usize;
    while cursor < bytes.len() {
        steps += 1;
        if steps.is_multiple_of(CANCELLATION_POLL_INTERVAL) {
            builder.check_cancelled()?;
        }
        // Words start on an ASCII byte or a UTF-8 lead byte and run through
        // every non-ASCII byte, so `cursor..end` is always a char range.
        if !is_word_byte(bytes[cursor]) || (cursor > 0 && is_word_byte(bytes[cursor - 1])) {
            cursor += 1;
            continue;
        }
        let end = cursor
            + bytes[cursor..]
                .iter()
                .take_while(|byte| is_word_byte(**byte))
                .count();
        if is_assignment_target(code, end)
            && let Some((_, count)) = counts
                .iter_mut()
                .find(|(name, _)| *name == &code[cursor..end])
        {
            *count = count.saturating_add(1);
        }
        cursor = end;
    }
    Ok(counts)
}

/// Whether an identifier ending at `end` is followed by `=` or `+=` (not `==`).
fn is_assignment_target(code: &str, end: usize) -> bool {
    let rest = &code[skip_ascii_whitespace(code, end)..];
    let rest = rest.strip_prefix('+').unwrap_or(rest);
    rest.starts_with('=') && !rest.starts_with("==")
}

/// Byte spans of the walker's field declarations, by field name.
///
/// Field spans prove that a one-line `String NAME = ...;` declaration is a
/// class-level field rather than an initializer-block local.
struct FieldSpans<'builder> {
    spans: HashMap<&'builder str, Vec<(usize, usize)>>,
}

impl<'builder> FieldSpans<'builder> {
    /// Index every original `Field` symbol's span by its name.
    fn new(builder: &'builder FrameworkBuilder<'_, '_>) -> Self {
        let mut spans = HashMap::<&str, Vec<(usize, usize)>>::new();
        let fields = (0..builder.original_symbol_count())
            .filter_map(|index| builder.original_symbol(index))
            .filter(|symbol| symbol.kind == SymbolKind::Field);
        for field in fields {
            if let (Ok(start), Ok(end)) = (
                usize::try_from(field.span.start_byte()),
                usize::try_from(field.span.end_byte()),
            ) {
                spans
                    .entry(field.name.as_str())
                    .or_default()
                    .push((start, end));
            }
        }
        Self { spans }
    }

    /// Whether a field named like the declaration spans its `=` and ends on
    /// the declaration's own line. A field whose initializer merely contains
    /// the line (an anonymous class's initializer local, for example) ends
    /// later and does not count.
    fn declares(&self, declaration: &ConstantDeclaration<'_>) -> bool {
        let offset = declaration.equals_offset;
        self.spans
            .get(declaration.constant.name)
            .is_some_and(|spans| {
                spans.iter().any(|(start, end)| {
                    *start <= offset && offset < *end && *end <= declaration.line_end
                })
            })
    }
}

/// A walker method or function and its byte span.
struct EnclosingMethod {
    id: SymbolId,
    start: usize,
    end: usize,
}

/// The walker's methods and functions sorted by span, answering innermost
/// containment for nondecreasing offsets in one amortized forward sweep.
struct CallableIndex {
    callables: Vec<EnclosingMethod>,
    next: usize,
    open: Vec<usize>,
}

impl CallableIndex {
    fn new(builder: &FrameworkBuilder<'_, '_>) -> Self {
        let mut callables = (0..builder.original_symbol_count())
            .filter_map(|index| builder.original_symbol(index))
            .filter(|symbol| matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function))
            .filter_map(|symbol| {
                Some(EnclosingMethod {
                    id: symbol.id.clone(),
                    start: usize::try_from(symbol.span.start_byte()).ok()?,
                    end: usize::try_from(symbol.span.end_byte()).ok()?,
                })
            })
            .collect::<Vec<_>>();
        callables.sort_by_key(|callable| (callable.start, Reverse(callable.end)));
        Self {
            callables,
            next: 0,
            open: Vec::new(),
        }
    }

    /// The innermost callable containing `offset`; offsets must not decrease
    /// between calls.
    fn innermost(&mut self, offset: usize) -> Option<&EnclosingMethod> {
        while let Some(start) = self
            .callables
            .get(self.next)
            .map(|callable| callable.start)
            .filter(|start| *start <= offset)
        {
            self.close_before(start);
            self.open.push(self.next);
            self.next += 1;
        }
        self.close_before(offset);
        self.open.last().map(|index| &self.callables[*index])
    }

    /// Drop open spans that end at or before `position`.
    fn close_before(&mut self, position: usize) {
        while self
            .open
            .last()
            .is_some_and(|index| self.callables[*index].end <= position)
        {
            self.open.pop();
        }
    }
}

/// Per-method cache of the names the method binds itself
/// (parameters, locals, loop and lambda variables), computed once per method.
struct MethodBindings<'scan> {
    code: &'scan str,
    methods: Vec<(SymbolId, Option<HashMap<&'scan str, usize>>)>,
}

impl<'scan> MethodBindings<'scan> {
    fn new(code: &'scan str) -> Self {
        Self {
            code,
            methods: Vec::new(),
        }
    }

    /// Cache the method's bindings; unknown or exhausted scopes abstain.
    fn bindings(
        &mut self,
        method: &EnclosingMethod,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<Option<&HashMap<&'scan str, usize>>, ExtractError> {
        let index = if let Some(index) = self.methods.iter().position(|(id, _)| *id == method.id) {
            index
        } else {
            let bound = if method.end.saturating_sub(method.start) <= MAX_SCOPE_BYTES
                && let Some(text) = self.code.get(method.start..method.end)
            {
                let mut scan = BindingScan::default();
                scan.find(text, check_cancelled)?.then_some(scan.bound)
            } else {
                None
            };
            self.methods.push((method.id.clone(), bound));
            self.methods.len() - 1
        };
        Ok(self.methods[index].1.as_ref())
    }
}

/// Whether a constant term is shadowed, or its bindings could not be proved.
fn shadows(bound: Option<&HashMap<&str, usize>>, expression: &str) -> bool {
    concatenation_terms(expression)
        .into_iter()
        .map(strip_parentheses)
        .filter(|term| is_identifier(term))
        .any(|term| bound.is_none_or(|names| names.contains_key(term)))
}

/// A method token, with the provisional declaration mark for an identifier.
#[derive(Clone, Copy)]
enum BindingLexeme<'scan> {
    Word { name: &'scan str, declared: bool },
    Punctuation(char),
}

impl BindingLexeme<'_> {
    fn width(self) -> usize {
        match self {
            Self::Word { name, .. } => name.len(),
            Self::Punctuation(character) => character.len_utf8(),
        }
    }
}

/// The last non-whitespace token; parenthesis spans index already-read tokens.
#[derive(Clone, Copy, Default)]
enum BindingToken {
    Word(usize),
    Closed {
        start: usize,
        end: usize,
        switch: bool,
    },
    Arrow,
    Dots(u8),
    TypeEnd,
    #[default]
    Other,
}

impl BindingToken {
    /// Doubt about a preceding word, generic, or array counts as a type.
    fn may_be_type(self, tokens: &[BindingLexeme<'_>]) -> bool {
        match self {
            Self::Word(index) => {
                let BindingLexeme::Word { name, .. } = tokens[index] else {
                    return false;
                };
                name.chars()
                    .next()
                    .is_some_and(|first| !first.is_ascii_digit())
                    && !STATEMENT_KEYWORDS.contains(&name)
            }
            Self::TypeEnd => true,
            Self::Dots(count) => count >= 3,
            Self::Arrow | Self::Closed { .. } | Self::Other => false,
        }
    }
}

/// A block's delimiter depth distinguishes switch labels from nested lambdas.
struct BindingBlock {
    parenthesis: usize,
    bracket: usize,
    switch: bool,
    label: Option<SwitchLabel>,
}

#[derive(Clone, Copy)]
struct SwitchLabel {
    start: usize,
    default: bool,
}

/// One forward pass through literal-masked method code. Switch labels and
/// lambda parameters are revisited only at their own separator.
#[derive(Default)]
struct BindingScan<'scan> {
    tokens: Vec<BindingLexeme<'scan>>,
    open: Vec<(usize, bool)>,
    blocks: Vec<BindingBlock>,
    bracket: usize,
    previous: BindingToken,
    bound: HashMap<&'scan str, usize>,
    work: usize,
}

impl<'scan> BindingScan<'scan> {
    fn find(
        &mut self,
        text: &'scan str,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<bool, ExtractError> {
        check_cancelled()?;
        let mut word_start = None;
        for (offset, character) in text.char_indices() {
            if !self.charge(character.len_utf8(), check_cancelled)? {
                return Ok(false);
            }
            if is_word_char(character) && !character.is_whitespace() {
                word_start.get_or_insert(offset);
                continue;
            }
            if let Some(start) = word_start.take() {
                self.word(&text[start..offset]);
            }
            if character.is_whitespace() {
                continue;
            }
            if Self::is_separator(character, text.as_bytes().get(offset + 1).copied())
                && !self.separator(character, check_cancelled)?
            {
                return Ok(false);
            }
            self.punctuation(character, text.as_bytes().get(offset + 1).copied());
        }
        if let Some(start) = word_start {
            self.word(&text[start..]);
        }
        Ok(true)
    }

    fn is_separator(character: char, next: Option<u8>) -> bool {
        character == ':' || character == '-' && next == Some(b'>')
    }

    fn word(&mut self, word: &'scan str) {
        self.begin_label(word);
        let declared = self.previous.may_be_type(&self.tokens);
        if declared {
            self.bind(word);
        }
        self.previous = BindingToken::Word(self.tokens.len());
        self.tokens.push(BindingLexeme::Word {
            name: word,
            declared,
        });
    }

    fn punctuation(&mut self, character: char, next: Option<u8>) {
        self.block_delimiter(character);
        self.previous = match character {
            '(' => {
                let switch = matches!(self.previous, BindingToken::Word(index)
                    if matches!(self.tokens[index], BindingLexeme::Word { name: "switch", .. }));
                self.open.push((self.tokens.len() + 1, switch));
                BindingToken::Other
            }
            ')' => self
                .open
                .pop()
                .map_or(BindingToken::Other, |(start, switch)| {
                    BindingToken::Closed {
                        start,
                        end: self.tokens.len(),
                        switch,
                    }
                }),
            '.' => BindingToken::Dots(match self.previous {
                BindingToken::Dots(count) => count.saturating_add(1),
                _ => 1,
            }),
            '-' if next == Some(b'>') => BindingToken::Arrow,
            '>' if matches!(self.previous, BindingToken::Arrow) => BindingToken::Other,
            '>' | ']' => BindingToken::TypeEnd,
            _ => BindingToken::Other,
        };
        self.tokens.push(BindingLexeme::Punctuation(character));
    }

    fn block_delimiter(&mut self, character: char) {
        match character {
            '{' => self.blocks.push(BindingBlock {
                parenthesis: self.open.len(),
                bracket: self.bracket,
                switch: matches!(self.previous, BindingToken::Closed { switch: true, .. }),
                label: None,
            }),
            '}' => {
                self.blocks.pop();
            }
            '[' => self.bracket += 1,
            ']' => self.bracket = self.bracket.saturating_sub(1),
            _ => {}
        }
    }

    fn switch_block(&mut self) -> Option<&mut BindingBlock> {
        self.blocks.last_mut().filter(|block| {
            block.switch && block.parenthesis == self.open.len() && block.bracket == self.bracket
        })
    }

    fn begin_label(&mut self, word: &str) {
        if !matches!(word, "case" | "default") {
            return;
        }
        let start = self.tokens.len() + 1;
        if let Some(block) = self.switch_block()
            && block.label.is_none()
        {
            block.label = Some(SwitchLabel {
                start,
                default: word == "default",
            });
        }
    }

    fn separator(
        &mut self,
        character: char,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<bool, ExtractError> {
        if let Some(label) = self.switch_block().and_then(|block| block.label.take()) {
            return self.switch_label(label, check_cancelled);
        }
        if character == '-' {
            self.lambda(check_cancelled)
        } else {
            Ok(true)
        }
    }

    /// Constant lists bind nothing; a supported type pattern binds one name.
    /// Every other pattern makes the method's complete binding set unknown.
    fn switch_label(
        &mut self,
        label: SwitchLabel,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<bool, ExtractError> {
        let end = self.tokens.len();
        if label.default {
            return Ok(label.start == end);
        }
        let mut entry = SwitchEntry::default();
        let mut multiple = false;
        for index in label.start..end {
            let Some(token) = self.binding_token(index, check_cancelled)? else {
                return Ok(false);
            };
            if entry.parameter.separator(token) {
                if !matches!(entry.finish(), SwitchBinding::Constant) {
                    return Ok(false);
                }
                multiple = true;
                entry = SwitchEntry::default();
            } else {
                entry.accept(token);
            }
        }
        match entry.finish() {
            SwitchBinding::Constant => Ok(true),
            SwitchBinding::Pattern(name) if !multiple => {
                self.bind(name);
                Ok(true)
            }
            SwitchBinding::Pattern(_) | SwitchBinding::Unsupported => Ok(false),
        }
    }

    /// Replace provisional declarations with the actual lambda binding names.
    fn lambda(
        &mut self,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<bool, ExtractError> {
        let parameters = match self.previous {
            BindingToken::Word(index) => index..index + 1,
            BindingToken::Closed { start, end, .. } => start..end,
            BindingToken::Arrow
            | BindingToken::TypeEnd
            | BindingToken::Dots(_)
            | BindingToken::Other => return Ok(true),
        };
        if parameters.is_empty() {
            return Ok(true);
        }
        let mut parameter = LambdaParameter::default();
        for index in parameters {
            let Some(token) = self.binding_token(index, check_cancelled)? else {
                return Ok(false);
            };
            if parameter.separator(token) {
                let Some(name) = parameter.take_name() else {
                    return Ok(false);
                };
                self.bind(name);
            } else {
                parameter.accept(token);
            }
        }
        let Some(name) = parameter.take_name() else {
            return Ok(false);
        };
        self.bind(name);
        Ok(true)
    }

    fn bind(&mut self, name: &'scan str) {
        *self.bound.entry(name).or_default() += 1;
    }

    /// Reclassify an already-read token once its binding syntax is known.
    fn binding_token(
        &mut self,
        index: usize,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<Option<BindingLexeme<'scan>>, ExtractError> {
        let token = self.tokens[index];
        if !self.charge(token.width(), check_cancelled)? {
            return Ok(None);
        }
        self.remove_provisional(index);
        Ok(Some(token))
    }

    /// Counts preserve same-named declarations elsewhere in the method.
    fn remove_provisional(&mut self, index: usize) {
        let BindingLexeme::Word {
            name,
            declared: true,
        } = self.tokens[index]
        else {
            return;
        };
        self.tokens[index] = BindingLexeme::Word {
            name,
            declared: false,
        };
        if let Some(count) = self.bound.get_mut(name) {
            *count -= 1;
            if *count == 0 {
                self.bound.remove(name);
            }
        }
    }

    /// Charge byte visits and poll even while inspecting a parameter list.
    fn charge(
        &mut self,
        bytes: usize,
        check_cancelled: &mut dyn FnMut() -> Result<(), ExtractError>,
    ) -> Result<bool, ExtractError> {
        let Some(work) = self
            .work
            .checked_add(bytes)
            .filter(|work| *work <= MAX_SCOPE_WORK)
        else {
            return Ok(false);
        };
        let first_poll = self.work / CANCELLATION_POLL_INTERVAL + 1;
        self.work = work;
        for _ in first_poll..=work / CANCELLATION_POLL_INTERVAL {
            check_cancelled()?;
        }
        Ok(true)
    }
}

/// Position in an annotation's qualified name and optional argument list.
#[derive(Default)]
enum LambdaAnnotation {
    #[default]
    None,
    Name,
    Suffix,
}

enum SwitchBinding<'scan> {
    Constant,
    Pattern(&'scan str),
    Unsupported,
}

/// Constant labels and type patterns share the parameter's annotation and
/// delimiter handling; only type patterns have two identifier groups.
#[derive(Default)]
struct SwitchEntry<'scan> {
    constant: LabelConstant,
    parameter: LambdaParameter<'scan>,
    invalid_pattern: bool,
}

impl<'scan> SwitchEntry<'scan> {
    fn accept(&mut self, token: BindingLexeme<'scan>) {
        self.constant.accept(token);
        if self.parameter.at_top_level() {
            self.invalid_pattern |= match token {
                BindingLexeme::Word { name, .. } => matches!(name, "case" | "default" | "when"),
                BindingLexeme::Punctuation(character) => {
                    !matches!(character, '.' | '@' | '<' | '[' | '(')
                        || self.parameter.groups >= 2 && !matches!(character, '@' | '(')
                }
            };
        }
        self.parameter.accept(token);
    }

    fn finish(&mut self) -> SwitchBinding<'scan> {
        if self.constant.valid() {
            return SwitchBinding::Constant;
        }
        if !self.invalid_pattern && self.parameter.groups == 2 {
            return self
                .parameter
                .take_name()
                .map_or(SwitchBinding::Unsupported, SwitchBinding::Pattern);
        }
        SwitchBinding::Unsupported
    }
}

/// Literal contents are already masked; quotes remain for literal labels.
#[derive(Default)]
enum LabelConstant {
    #[default]
    Start,
    Name,
    Dot,
    Signed,
    Number,
    Quoted {
        quote: char,
        count: usize,
    },
    Invalid,
}

impl LabelConstant {
    fn accept(&mut self, token: BindingLexeme<'_>) {
        match token {
            BindingLexeme::Word { name, .. } => self.word(name),
            BindingLexeme::Punctuation(character) => self.punctuation(character),
        }
    }

    fn word(&mut self, name: &str) {
        let number = name.starts_with(|character: char| character.is_ascii_digit());
        *self = if matches!(self, Self::Start | Self::Dot)
            && !number
            && !matches!(name, "final" | "when")
        {
            Self::Name
        } else if matches!(self, Self::Start | Self::Signed) && number {
            Self::Number
        } else {
            Self::Invalid
        };
    }

    fn punctuation(&mut self, character: char) {
        *self = match (&*self, character) {
            (Self::Start, '+' | '-') => Self::Signed,
            (Self::Name, '.') => Self::Dot,
            (Self::Start, '\'' | '"') => Self::Quoted {
                quote: character,
                count: 1,
            },
            (Self::Quoted { quote, count }, character) if character == *quote => Self::Quoted {
                quote: *quote,
                count: count + 1,
            },
            _ => Self::Invalid,
        };
    }

    fn valid(&self) -> bool {
        matches!(
            self,
            Self::Name
                | Self::Number
                | Self::Quoted { count: 2, .. }
                | Self::Quoted {
                    quote: '"',
                    count: 6
                }
        )
    }
}

/// A parameter ends at a comma outside generic, parenthesis and array depth.
/// Its last identifier, excluding annotations and `final`, is the binding;
/// trailing `...` and `[]` contain no identifiers and leave that name intact.
#[derive(Default)]
struct LambdaParameter<'scan> {
    angle: usize,
    parenthesis: usize,
    bracket: usize,
    annotation: LambdaAnnotation,
    last: Option<&'scan str>,
    invalid: bool,
    groups: usize,
    qualified: bool,
}

impl<'scan> LambdaParameter<'scan> {
    fn at_top_level(&self) -> bool {
        self.angle == 0 && self.parenthesis == 0 && self.bracket == 0
    }

    fn separator(&self, token: BindingLexeme<'_>) -> bool {
        matches!(token, BindingLexeme::Punctuation(',')) && self.at_top_level()
    }

    fn accept(&mut self, token: BindingLexeme<'scan>) {
        match token {
            BindingLexeme::Word { name, .. } => self.word(name),
            BindingLexeme::Punctuation(character) => self.punctuation(character),
        }
    }

    fn word(&mut self, name: &'scan str) {
        if !self.at_top_level() {
            return;
        }
        if matches!(self.annotation, LambdaAnnotation::Name) {
            self.annotation = LambdaAnnotation::Suffix;
            return;
        }
        self.annotation = LambdaAnnotation::None;
        if name != "final"
            && name
                .chars()
                .next()
                .is_some_and(|first| !first.is_ascii_digit())
        {
            if !self.qualified {
                self.groups += 1;
            }
            self.qualified = false;
            self.last = Some(name);
        }
    }

    fn punctuation(&mut self, character: char) {
        if self.at_top_level() {
            // A record-pattern switch arm can also precede an arrow. Its
            // nested, non-annotation parentheses are not lambda parameters.
            if character == '(' && !matches!(self.annotation, LambdaAnnotation::Suffix) {
                self.invalid = true;
            }
            if character == '.' && matches!(self.annotation, LambdaAnnotation::Suffix) {
                self.annotation = LambdaAnnotation::Name;
                return;
            }
            if character == '.' {
                self.qualified = true;
            }
            self.annotation = if character == '@' {
                LambdaAnnotation::Name
            } else {
                LambdaAnnotation::None
            };
        }
        self.delimiter(character);
    }

    fn delimiter(&mut self, character: char) {
        match character {
            '(' => self.parenthesis += 1,
            ')' => self.parenthesis = self.parenthesis.saturating_sub(1),
            '[' => self.bracket += 1,
            ']' => self.bracket = self.bracket.saturating_sub(1),
            '<' if self.parenthesis == 0 && self.bracket == 0 => self.angle += 1,
            '>' if self.parenthesis == 0 && self.bracket == 0 => {
                self.angle = self.angle.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn take_name(&mut self) -> Option<&'scan str> {
        if self.invalid || !self.at_top_level() || matches!(self.annotation, LambdaAnnotation::Name)
        {
            return None;
        }
        self.annotation = LambdaAnnotation::None;
        self.last.take()
    }
}

/// Keywords that can directly precede an expression identifier.
const STATEMENT_KEYWORDS: &[&str] = &[
    "return",
    "throw",
    "case",
    "new",
    "else",
    "yield",
    "assert",
    "do",
    "instanceof",
];

fn strip_parentheses(term: &str) -> &str {
    let mut term = term.trim();
    while let Some(inner) = term
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
    {
        term = inner.trim();
    }
    term
}

/// A one-line constant declaration, the offset of its `=` (which lies in the
/// same scope as the declared name), and the end offset of its line.
struct ConstantDeclaration<'source> {
    constant: StringConstant<'source>,
    equals_offset: usize,
    line_end: usize,
}

/// `[modifiers] String NAME = expression;` on one literal-masked code line
/// starting at `line_start` (v1 `parseStringConstantDeclaration`); the
/// expression is sliced from the unmasked source at the same offsets.
fn string_constant<'source>(
    text: JavaText<'source>,
    line_start: usize,
    line: &'source str,
) -> Option<ConstantDeclaration<'source>> {
    let equals = line.find('=')?;
    let mut words = line[..equals].split_whitespace();
    words.by_ref().find(|word| *word == "String")?;
    let name = words.next()?;
    if words.next().is_some() || !is_identifier(name) {
        return None;
    }
    let rest = &line[equals + 1..];
    let terminator = rest.trim_end().strip_suffix(';')?;
    let leading = terminator.len() - terminator.trim_start().len();
    let expression_start = line_start + equals + 1 + leading;
    let expression_end = line_start + equals + 1 + terminator.trim_end().len();
    Some(ConstantDeclaration {
        constant: StringConstant {
            name,
            expression: text.source.get(expression_start..expression_end)?,
        },
        equals_offset: line_start + equals,
        line_end: line_start + line.len(),
    })
}

/// First arguments of every `getSqlSessionTemplate().m(` / `sqlSessionTemplate.m(`
/// call in code (never inside literals), sorted by call offset.
fn template_arguments<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    text: JavaText<'source>,
) -> Result<Vec<TemplateArgument<'source>>, ExtractError> {
    let code = text.code;
    let mut arguments = Vec::new();
    for (receiver, invoked) in TEMPLATE_RECEIVERS {
        let mut cursor = 0;
        while let Some(relative) = code[cursor..].find(receiver) {
            builder.check_cancelled()?;
            let start = cursor + relative;
            cursor = start + receiver.len();
            if start > 0 && is_word_byte(code.as_bytes()[start - 1]) {
                continue;
            }
            if let Some((argument_start, length)) = template_call_argument(code, cursor, *invoked)
                && let Some(expression) = text.source.get(argument_start..argument_start + length)
            {
                arguments.push(TemplateArgument {
                    expression: expression.trim_end(),
                    start: argument_start,
                    call: start,
                });
            }
        }
    }
    arguments.sort_by_key(|argument| argument.call);
    arguments.truncate(MAX_TEMPLATE_CALLS);
    Ok(arguments)
}

/// Java literal context at the tracker's cursor.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum LiteralState {
    #[default]
    Code,
    String,
    TextBlock,
    Character,
}

/// Forward-only Java literal state (`"..."`, `'"'`, and `"""` text blocks).
#[derive(Default)]
struct LiteralTracker {
    state: LiteralState,
    escaped: bool,
}

impl LiteralTracker {
    /// Consume the token at `index` and return how many bytes it spans.
    fn advance(&mut self, bytes: &[u8], index: usize) -> usize {
        let byte = bytes[index];
        if self.escaped {
            self.escaped = false;
            return 1;
        }
        let text_block = bytes.get(index..index + TEXT_BLOCK_QUOTE.len()) == Some(TEXT_BLOCK_QUOTE);
        match (self.state, byte) {
            (LiteralState::Code, b'"') if text_block => {
                self.state = LiteralState::TextBlock;
                return TEXT_BLOCK_QUOTE.len();
            }
            (LiteralState::TextBlock, b'"') if text_block => {
                self.state = LiteralState::Code;
                return TEXT_BLOCK_QUOTE.len();
            }
            (LiteralState::Code, b'"') => self.state = LiteralState::String,
            (LiteralState::Code, b'\'') => self.state = LiteralState::Character,
            (LiteralState::String | LiteralState::Character | LiteralState::TextBlock, b'\\') => {
                self.escaped = true;
            }
            (LiteralState::String, b'"' | b'\n') | (LiteralState::Character, b'\'' | b'\n') => {
                self.state = LiteralState::Code;
            }
            _ => {}
        }
        1
    }
}

/// Parse `[()] . method (` after a receiver in literal-masked code and return
/// the first argument's start offset and byte length.
fn template_call_argument(code: &str, mut cursor: usize, invoked: bool) -> Option<(usize, usize)> {
    if invoked {
        cursor = expect_byte(code, cursor, b'(')?;
        cursor = expect_byte(code, cursor, b')')?;
    }
    cursor = expect_byte(code, cursor, b'.')?;
    cursor = skip_ascii_whitespace(code, cursor);
    let method = TEMPLATE_METHODS.iter().find(|method| {
        code[cursor..].starts_with(**method)
            && !code
                .as_bytes()
                .get(cursor + method.len())
                .copied()
                .is_some_and(is_word_byte)
    })?;
    cursor = expect_byte(code, cursor + method.len(), b'(')?;
    let start = skip_ascii_whitespace(code, cursor);
    let length = first_argument_length(code.get(start..)?)?;
    Some((start, length))
}

fn expect_byte(code: &str, cursor: usize, expected: u8) -> Option<usize> {
    let cursor = skip_ascii_whitespace(code, cursor);
    (code.as_bytes().get(cursor) == Some(&expected)).then_some(cursor + 1)
}

/// Bytes up to the first top-level `,` or `)` (v1 `readFirstArgument`).
fn first_argument_length(value: &str) -> Option<usize> {
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate().take(MAX_ARGUMENT_BYTES) {
        if crate::framework::consume_quoted_byte(byte, &mut quote, &mut escaped) {
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'(' | b'[' | b'{' => depth = depth.saturating_add(1),
            b')' | b',' if depth == 0 => return Some(index),
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Evaluate a `+` concatenation of literals, known constants, and
/// `X.class.getName()`; `None` when any term is not exactly known.
fn evaluate_expression(expression: &str, constants: &Constants<'_>) -> Option<String> {
    let mut value = String::new();
    for term in concatenation_terms(expression) {
        let term = evaluate_term(term, constants)?;
        if value.len().saturating_add(term.len()) > MAX_VALUE_BYTES {
            return None;
        }
        value.push_str(term);
    }
    (!value.is_empty()).then_some(value)
}

fn concatenation_terms(expression: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, byte) in expression.bytes().enumerate() {
        if crate::framework::consume_quoted_byte(byte, &mut quote, &mut escaped) {
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'(' => depth = depth.saturating_add(1),
            b')' => depth = depth.saturating_sub(1),
            b'+' if depth == 0 => {
                terms.push(expression[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    terms.push(expression[start..].trim());
    terms
}

fn evaluate_term<'value>(
    term: &'value str,
    constants: &'value Constants<'_>,
) -> Option<&'value str> {
    let term = strip_parentheses(term);
    if let Some(literal) = term
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return (!literal.contains(['"', '\\'])).then_some(literal);
    }
    if let Some(value) = constants.get(term) {
        return Some(value);
    }
    let class = CLASS_NAME_SUFFIXES
        .iter()
        .find_map(|suffix| term.strip_suffix(suffix))?;
    let simple = class.rsplit('.').next()?;
    (class.split('.').all(is_identifier)).then_some(simple)
}

/// `a.b.Mapper.stmt` -> `Mapper::stmt` (v1 `toXmlStatementQualifiedName`).
fn statement_qualified_name(statement_id: &str) -> Option<String> {
    let (mapper, statement) = if let Some((mapper, statement)) = statement_id.rsplit_once("::") {
        (mapper.rsplit("::").next()?, statement)
    } else {
        let mut parts = statement_id.rsplit('.').filter(|part| !part.is_empty());
        let statement = parts.next()?;
        (parts.next()?, statement)
    };
    (is_identifier(mapper) && is_identifier(statement)).then(|| format!("{mapper}::{statement}"))
}

/// An ASCII Java identifier; only these become constant or statement names.
fn is_identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$'))
        && value
            .bytes()
            .all(|byte| byte.is_ascii() && is_word_byte(byte))
}

/// Identifier bytes for word boundaries: ASCII identifier characters and every
/// non-ASCII byte, so a boundary never splits a multibyte identifier.
const fn is_word_byte(byte: u8) -> bool {
    !byte.is_ascii() || byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

/// The `char` form of [`is_word_byte`].
fn is_word_char(character: char) -> bool {
    !character.is_ascii() || character.is_ascii_alphanumeric() || matches!(character, '_' | '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large_arguments() -> String {
        vec!["NS"; 16_000].join(",")
    }

    #[test]
    fn binding_scan_distinguishes_switch_arms_from_lambdas() {
        let cases: &[(&str, &[&str])] = &[
            ("switch (input) { case NS -> call(NS); default -> {} }", &[]),
            ("switch (input) { default -> call(NS); }", &[]),
            (
                "switch (input) { case Map.NS, OTHER, null, default -> call(NS); }",
                &[],
            ),
            (
                "switch (input) { case -1, ' ', \"\" -> call(NS); default -> {} }",
                &[],
            ),
            ("switch (input) { case String NS -> call(NS); }", &["NS"]),
            (
                "switch (input) { case @pkg.NS(value = {1, 2}) final java.lang.String NS -> call(NS); }",
                &["NS"],
            ),
            (
                "switch (input) { case Map<String, NS> value -> call(NS); default -> {} }",
                &["value"],
            ),
            (
                "switch (input) { case java.lang.@NS String value -> call(NS); default -> {} }",
                &["value"],
            ),
            (
                "switch (input) { case NS -> consume(NS -> call(NS)); default -> {} }",
                &["NS"],
            ),
            (
                "switch (input) { case NS: consume(NS -> call(NS)); break; default: break; }",
                &["NS"],
            ),
            (
                "switch (input) { case NS -> { switch (other) { case Map.NS -> call(NS); default -> {} } } default -> {} }",
                &[],
            ),
            ("NS -> call(NS)", &["NS"]),
        ];
        for (text, expected) in cases {
            let mut scan = BindingScan::default();
            assert_eq!(scan.find(text, &mut || Ok(())), Ok(true), "{text}");
            let mut actual = scan.bound.keys().copied().collect::<Vec<_>>();
            actual.sort_unstable();
            assert_eq!(actual, *expected, "{text}");
        }
    }

    #[test]
    fn binding_scan_abstains_for_unsupported_switch_patterns() {
        for label in [
            "Box(String value)",
            "Outer(Inner(String NS))",
            "String value when value.isEmpty()",
            "String value, NS",
            "NS + OTHER",
            "(NS)",
        ] {
            let text = format!("switch (input) {{ case {label} -> call(NS); default -> {{}} }}");
            let mut scan = BindingScan::default();
            assert_eq!(scan.find(&text, &mut || Ok(())), Ok(false), "{label}");
            assert!(shadows(None, "NS + \".find\""));
        }
    }

    #[test]
    fn binding_scan_switch_labels_have_bounded_cancellable_work() {
        let arguments = large_arguments();
        let text = format!("switch (input) {{ case {arguments} -> call(NS); default -> {{}} }}");
        assert!(text.len() < MAX_SCOPE_BYTES);
        let mut scan = BindingScan::default();
        assert_eq!(scan.find(&text, &mut || Ok(())), Ok(true));
        assert_eq!(scan.work, text.len() + arguments.len());
        assert!(!shadows(Some(&scan.bound), "NS + \".find\""));
        let cancel_at = 2 + text.len() / CANCELLATION_POLL_INTERVAL;
        let mut polls = 0;
        let mut cancelled = BindingScan::default();
        assert_eq!(
            cancelled.find(&text, &mut || {
                polls += 1;
                if polls == cancel_at {
                    Err(ExtractError::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(ExtractError::Cancelled)
        );
        assert!(cancelled.work > text.len());
        assert!(cancelled.work <= text.len() + CANCELLATION_POLL_INTERVAL);
    }

    #[test]
    fn binding_scan_collects_only_lambda_parameter_names() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("NS, value", &["NS", "value"]),
            ("Map<String, NS> values", &["values"]),
            ("final java.util.Map<String, List<NS>> values", &["values"]),
            ("Map<? super NS, ? extends Map> values", &["values"]),
            ("@Map final Map<String, Integer> values", &["values"]),
            (
                "@qualified.NS(flag = (LEFT < RIGHT)) String values",
                &["values"],
            ),
            (
                "Map<@Marker(value = {1, 2}) String, NS> values, @NS final String... last",
                &["last", "values"],
            ),
            ("String NS[]", &["NS"]),
            ("String values @NS []", &["values"]),
            ("var café, var NS", &["NS", "café"]),
        ];
        for (parameters, expected) in cases {
            let text = format!("({parameters}) -> call()");
            let mut scan = BindingScan::default();
            assert_eq!(scan.find(&text, &mut || Ok(())), Ok(true), "{parameters}");
            let mut actual = scan.bound.keys().copied().collect::<Vec<_>>();
            actual.sort_unstable();
            assert_eq!(actual, *expected, "{parameters}");
        }
        let mut bare = BindingScan::default();
        assert_eq!(bare.find("NS -> call(NS)", &mut || Ok(())), Ok(true));
        assert_eq!(bare.bound.keys().copied().collect::<Vec<_>>(), ["NS"]);
    }

    #[test]
    fn binding_scan_lambda_types_preserve_bindings_elsewhere() {
        let text = "void load(String NS) { c = (@NS final Map<String, NS> values) -> call(NS); }";
        let mut scan = BindingScan::default();
        assert_eq!(scan.find(text, &mut || Ok(())), Ok(true));
        assert!(shadows(Some(&scan.bound), "NS + \".find\""));
        assert!(!shadows(Some(&scan.bound), "Map + \".find\""));
        assert!(scan.bound.contains_key("values"));
    }

    #[test]
    fn binding_scan_lambda_body_identifiers_are_not_type_names() {
        let text = "(Object value) -> NS == null ? select(NS) : fallback()";
        let mut scan = BindingScan::default();
        assert_eq!(scan.find(text, &mut || Ok(())), Ok(true));
        assert!(!shadows(Some(&scan.bound), "NS + \".find\""));
        assert!(scan.bound.contains_key("value"));
    }

    #[test]
    fn binding_scan_large_argument_list_has_linear_work() {
        let text = format!(
            "void load() {{ consume({}); sqlSessionTemplate.selectOne(NS + \".find\"); }}",
            large_arguments()
        );
        assert!(text.len() < MAX_SCOPE_BYTES);
        let mut scan = BindingScan::default();
        let mut polls = 0;
        assert_eq!(
            scan.find(&text, &mut || {
                polls += 1;
                Ok(())
            }),
            Ok(true)
        );
        assert_eq!(scan.work, text.len(), "each source byte is charged once");
        assert_eq!(polls, 1 + text.len() / CANCELLATION_POLL_INTERVAL);
        assert!(!shadows(Some(&scan.bound), "NS + \".find\""));
    }

    #[test]
    fn binding_scan_propagates_cancellation_during_forward_scan() {
        let text = format!("consume({})", large_arguments());
        let mut scan = BindingScan::default();
        let mut polls = 0;
        assert_eq!(
            scan.find(&text, &mut || {
                polls += 1;
                if polls == 3 {
                    Err(ExtractError::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(ExtractError::Cancelled)
        );
        assert_eq!(scan.work, 2 * CANCELLATION_POLL_INTERVAL);
    }

    #[test]
    fn binding_scan_propagates_cancellation_during_lambda_parameters() {
        let text = format!("({}) ->", large_arguments());
        let mut scan = BindingScan::default();
        let mut polls = 0;
        let cancel_at = 2 + text.len() / CANCELLATION_POLL_INTERVAL;
        assert_eq!(
            scan.find(&text, &mut || {
                polls += 1;
                if polls == cancel_at {
                    Err(ExtractError::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(ExtractError::Cancelled)
        );
        assert!(
            scan.work > text.len(),
            "cancellation reached the parameter pass"
        );
        assert!(scan.work <= text.len() + CANCELLATION_POLL_INTERVAL);
    }

    #[test]
    fn binding_scan_abstains_when_overlapping_lists_exhaust_work() {
        let text = format!("{}NS{}", "(@A ".repeat(1_024), ") -> NS".repeat(1_024));
        assert!(text.len() < MAX_SCOPE_BYTES);
        let mut scan = BindingScan::default();
        assert_eq!(scan.find(&text, &mut || Ok(())), Ok(false));
        assert!(scan.work <= MAX_SCOPE_WORK);
        assert!(
            scan.work >= MAX_SCOPE_WORK - 2,
            "the work ceiling caused abstention"
        );
        assert!(shadows(None, "NS + \".find\""));
        assert!(!shadows(None, "\"pkg.Mapper.find\""));
    }
}
