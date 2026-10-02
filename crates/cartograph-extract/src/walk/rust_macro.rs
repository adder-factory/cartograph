//! Rust macro invocations and the references inside their token trees.
//!
//! Tree-sitter keeps macro arguments as an opaque `token_tree`, so the usage
//! walk never sees the calls and paths they contain. This scanner recovers the
//! shapes that equivalent non-macro code publishes, with the same reference
//! kind, name, owner, and resolution hint, so the resolver treats them alike:
//!
//! - `name(..)`, `a::b(..)`, `x.m(..)`, and `self.m(..)` calls;
//! - `a::B` and `Self::LIMIT` paths that are not called;
//! - nested `name!(..)` invocations, recorded as macro calls.
//!
//! A bare identifier that is not called is usually a local binding, which the
//! resolver cannot tell apart from a project declaration, so it stays
//! unrecorded as it does outside macros. The one exception is a
//! constant-shaped name (`MAX_ROWS`) inside a std formatting or assertion
//! macro, whose arguments are known expressions: Rust's naming lints reserve
//! that shape for constants and statics, so it is recorded as a value
//! reference both as an argument token and as an inline `{LIMIT}` /
//! `{value:WIDTH$}` capture in the format string. Other macros may be DSLs in
//! which such a token is a key, a type, or text, so they record none.
//!
//! Attribute bodies, nested `macro_rules!` definitions, `$` metavariables,
//! `#` interpolations, lifetimes, declared names, literals, and string text
//! outside a format string never produce references.

use cartograph_domain::{ReferenceKind, SourcePosition, SourceSpan};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference, RUST_MACRO_RESOLUTION_PREFIX};

use super::{
    ExtractionBuilder, references,
    syntax::{DirectChildren, children, named_children, span_for},
};

/// Most references one invocation's token tree may add; more fails the file's output bound.
/// A sweep of 55,866 crates.io sources peaked at 2,204 (generated FFI declarations), and
/// the per-file fact and output budgets still bound the total.
const MAX_RUST_MACRO_REFERENCES: usize = 8_192;
/// Emitted references between cancellation checks.
const REFERENCE_CANCELLATION_INTERVAL: usize = 64;
/// Visited tokens between cancellation checks, so a reference-free tree still polls.
const TOKEN_CANCELLATION_INTERVAL: usize = 256;
/// Single uppercase letters are usually generic or const parameters, not constants.
const MINIMUM_CONSTANT_NAME_BYTES: usize = 2;
const RUST_PATH_SEPARATOR: &[u8] = b"::";

/// Std formatting macros and the zero-based argument that holds their format string.
const RUST_FORMAT_MACROS: [(&str, usize); 20] = [
    ("format", 0),
    ("format_args", 0),
    ("print", 0),
    ("println", 0),
    ("eprint", 0),
    ("eprintln", 0),
    ("panic", 0),
    ("unreachable", 0),
    ("todo", 0),
    ("unimplemented", 0),
    ("write", 1),
    ("writeln", 1),
    ("assert", 1),
    ("debug_assert", 1),
    ("assert_eq", 2),
    ("assert_ne", 2),
    ("debug_assert_eq", 2),
    ("debug_assert_ne", 2),
    ("assert_matches", 2),
    ("debug_assert_matches", 2),
];

/// Reserved words a token tree lexes as identifiers; none names a value or callable.
const RUST_RESERVED_IDENTIFIERS: [&str; 19] = [
    "abstract",
    "become",
    "box",
    "do",
    "dyn",
    "else",
    "extern",
    "final",
    "in",
    "macro_rules",
    "move",
    "override",
    "priv",
    "ref",
    "try",
    "typeof",
    "unsized",
    "virtual",
    "yield",
];

/// Keywords after which the next identifier is being declared rather than used.
const RUST_DECLARATION_KEYWORDS: [&str; 13] = [
    "as", "const", "enum", "fn", "for", "let", "mod", "ref", "static", "struct", "trait", "type",
    "union",
];

/// Tokens after which an identifier continues an earlier path, receiver, lifetime,
/// metavariable, or quoted interpolation instead of starting a reference.
const RUST_CONTINUATION_TOKENS: [&str; 5] = [".", "::", "$", "'", "#"];

/// Record a macro invocation and the references its token tree carries.
pub(super) fn capture_invocation(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(target) = node.child_by_field_name("macro") else {
        return Ok(());
    };
    let name = builder.context.owned_text(target)?;
    let role = macro_role(&name);
    let reference = macro_call_reference(builder, name, span_for(target)?)?;
    builder.emit_reference(reference)?;
    let Some(tokens) = named_children(node).find(|child| child.kind() == "token_tree") else {
        return Ok(());
    };
    let source = builder.context.snapshot.source();
    MacroTokenScan {
        builder,
        source,
        limit: tokens.end_byte(),
        emitted: 0,
        visited: 0,
    }
    .scan(tokens, role)
}

fn macro_call_reference(
    builder: &ExtractionBuilder<'_, '_>,
    name: String,
    span: SourceSpan,
) -> Result<ExtractedReference, ExtractError> {
    let capacity = RUST_MACRO_RESOLUTION_PREFIX
        .len()
        .checked_add(name.len())
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(capacity)?;
    let mut resolution_name = String::new();
    resolution_name
        .try_reserve_exact(capacity)
        .map_err(|_| ExtractError::OutputLimit)?;
    resolution_name.push_str(RUST_MACRO_RESOLUTION_PREFIX);
    resolution_name.push_str(&name);
    Ok(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name: Some(resolution_name),
        kind: ReferenceKind::Calls,
        span,
    })
}

/// How the scan reads one token tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeRole {
    /// Input of a macro whose token syntax is unknown, such as a DSL, where only
    /// calls, paths, receiver calls, and nested invocations are unambiguous.
    Tokens,
    /// Expressions of a std formatting macro, or a group nested inside them. The
    /// format string, when this tree holds one, is at this argument index.
    Expressions(Option<usize>),
    /// An attribute body or nested `macro_rules!` definition: template text, not references.
    Skipped,
}

fn macro_role(macro_name: &str) -> TreeRole {
    let last_segment = macro_name.rsplit("::").next().unwrap_or(macro_name).trim();
    RUST_FORMAT_MACROS
        .iter()
        .find(|(name, _)| *name == last_segment)
        .map_or(TreeRole::Tokens, |(_, argument)| {
            TreeRole::Expressions(Some(*argument))
        })
}

struct TokenFrame<'tree> {
    tree: Node<'tree>,
    children: DirectChildren<'tree>,
    role: TreeRole,
    cursor: FrameCursor<'tree>,
}

/// Where the scan stands among one token tree's direct children.
#[derive(Default)]
struct FrameCursor<'tree> {
    /// Zero-based top-level argument, counted by direct `,` children.
    argument: usize,
    /// Whether the next significant token begins an argument.
    argument_start: bool,
    previous: Option<Node<'tree>>,
    before_previous: Option<Node<'tree>>,
    /// A literal that is the format string if its argument ends right after it.
    format_candidate: Option<Node<'tree>>,
    /// Start byte of the token tree that a preceding `name!` invokes, and its role.
    next_tree: Option<(usize, TreeRole)>,
}

impl<'tree> FrameCursor<'tree> {
    fn advance(&mut self, token: Node<'tree>) {
        let kind = token.kind();
        if kind == "," {
            self.argument = self.argument.saturating_add(1);
        }
        self.argument_start = opens_argument(kind);
        self.before_previous = self.previous.replace(token);
    }
}

fn opens_argument(kind: &str) -> bool {
    matches!(kind, "," | "(" | "[" | "{")
}

fn closes_argument(kind: &str) -> bool {
    matches!(kind, "," | ")" | "]" | "}")
}

/// What immediately follows an identifier or path inside a token tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follower {
    Call,
    /// `!` invoking a macro whose arguments start at this byte.
    Macro {
        arguments: usize,
    },
    /// A single `:` or `=`, which declares a field, binding, or named argument.
    Binding,
    Other,
}

impl Follower {
    fn read(bytes: &[u8], end: usize, limit: usize) -> Self {
        let at = skip_ascii_whitespace(bytes, end, limit);
        let next = at.saturating_add(1);
        let following = (next < limit).then(|| bytes.get(next).copied()).flatten();
        match (at < limit).then(|| bytes.get(at).copied()).flatten() {
            Some(b'(') => Self::Call,
            Some(b'!') if following != Some(b'=') => Self::Macro {
                arguments: skip_ascii_whitespace(bytes, next, limit),
            },
            Some(b':') if following != Some(b':') => Self::Binding,
            Some(b'=') if !matches!(following, Some(b'=' | b'>')) => Self::Binding,
            _ => Self::Other,
        }
    }
}

/// A nested `path!` invocation: where its path ends and its arguments begin.
#[derive(Clone, Copy)]
struct MacroPath {
    end: usize,
    arguments: usize,
}

/// An identifier-led path such as `a::b::c` and the token after it.
struct TokenShape {
    end: usize,
    segments: usize,
    follower: Follower,
}

impl TokenShape {
    fn read(bytes: &[u8], token: Node<'_>, limit: usize) -> Self {
        let mut end = token.end_byte();
        let mut segments = 0_usize;
        loop {
            let separator = skip_ascii_whitespace(bytes, end, limit);
            let component = separator.saturating_add(RUST_PATH_SEPARATOR.len());
            if component > limit || bytes.get(separator..component) != Some(RUST_PATH_SEPARATOR) {
                break;
            }
            let component = skip_ascii_whitespace(bytes, component, limit);
            let Some(component_end) = rust_component_end(bytes, component, limit) else {
                break;
            };
            end = component_end;
            segments = segments.saturating_add(1);
        }
        Self {
            end,
            segments,
            follower: Follower::read(bytes, end, limit),
        }
    }
}

struct MacroTokenScan<'builder, 'source, 'cancel> {
    builder: &'builder mut ExtractionBuilder<'source, 'cancel>,
    source: &'source str,
    /// End of the invocation's outermost token tree; lookahead never passes it.
    limit: usize,
    emitted: usize,
    visited: usize,
}

impl<'source> MacroTokenScan<'_, 'source, '_> {
    fn scan(mut self, tokens: Node<'_>, role: TreeRole) -> Result<(), ExtractError> {
        let mut frames = Vec::new();
        push_frame(&mut frames, tokens, role)?;
        while let Some(frame) = frames.last_mut() {
            let Some(child) = frame.children.next() else {
                frames.pop();
                continue;
            };
            self.poll_token()?;
            if let Some((tree, role)) = self.visit(frame, child)? {
                if frames.len() > self.builder.maximum_ast_depth {
                    return Err(ExtractError::NestingLimit);
                }
                push_frame(&mut frames, tree, role)?;
            }
        }
        Ok(())
    }

    /// Read one direct child, returning a nested token tree that should be scanned next.
    fn visit<'tree>(
        &mut self,
        frame: &mut TokenFrame<'tree>,
        token: Node<'tree>,
    ) -> Result<Option<(Node<'tree>, TreeRole)>, ExtractError> {
        if token.is_extra() {
            return Ok(None);
        }
        let kind = token.kind();
        if let Some(candidate) = frame.cursor.format_candidate.take()
            && closes_argument(kind)
        {
            self.capture_format_string(frame.tree, candidate)?;
        }
        let mut nested = None;
        match kind {
            "token_tree" => nested = nested_tree(frame, token),
            "string_literal" | "raw_string_literal"
                if frame.role == TreeRole::Expressions(Some(frame.cursor.argument))
                    && frame.cursor.argument_start =>
            {
                frame.cursor.format_candidate = Some(token);
            }
            "identifier" | "self" | "crate" | "super" => self.capture_token(frame, token)?,
            _ => {}
        }
        frame.cursor.advance(token);
        Ok(nested)
    }

    fn capture_token(
        &mut self,
        frame: &mut TokenFrame<'_>,
        token: Node<'_>,
    ) -> Result<(), ExtractError> {
        if continues_previous(self.source, &frame.cursor) {
            return Ok(());
        }
        let shape = TokenShape::read(self.source.as_bytes(), token, self.limit);
        if let Follower::Macro { arguments } = shape.follower {
            let path = MacroPath {
                end: shape.end,
                arguments,
            };
            return self.capture_nested_macro(frame, token, path);
        }
        if shape.segments > 0 {
            let kind = if shape.follower == Follower::Call {
                ReferenceKind::Calls
            } else {
                ReferenceKind::References
            };
            return self.emit_path(token, shape.end, kind);
        }
        self.capture_single_token(token, shape.follower, frame.role)
    }

    fn capture_nested_macro(
        &mut self,
        frame: &mut TokenFrame<'_>,
        token: Node<'_>,
        path: MacroPath,
    ) -> Result<(), ExtractError> {
        let name = self.compact_path(token.start_byte(), path.end)?;
        if name == "macro_rules" {
            // `macro_rules! name { .. }` defines a template; its body is not code.
            let bytes = self.source.as_bytes();
            frame.cursor.next_tree =
                rust_component_end(bytes, path.arguments, self.limit).map(|name_end| {
                    let body = skip_ascii_whitespace(bytes, name_end, self.limit);
                    (body, TreeRole::Skipped)
                });
            return Ok(());
        }
        frame.cursor.next_tree = Some((path.arguments, macro_role(&name)));
        let span = SourceCursor::at(self.source, token)?.span(token.start_byte(), path.end)?;
        let reference = macro_call_reference(self.builder, name, span)?;
        self.emit(reference)
    }

    fn capture_single_token(
        &mut self,
        token: Node<'_>,
        follower: Follower,
        role: TreeRole,
    ) -> Result<(), ExtractError> {
        let text = token_text(self.source, token);
        if RUST_RESERVED_IDENTIFIERS.contains(&text) {
            return Ok(());
        }
        if token.kind() == "self" {
            return self.capture_receiver_call(token);
        }
        if token.kind() != "identifier" {
            return Ok(());
        }
        if follower == Follower::Call {
            return self.emit_token(token, ReferenceKind::Calls);
        }
        self.capture_receiver_call(token)?;
        // Only a known expression position makes a constant-shaped token a value;
        // in a DSL it may be a key, a type, or text the macro interprets.
        if matches!(role, TreeRole::Expressions(_))
            && follower != Follower::Binding
            && constant_shaped(text)
        {
            self.emit_token(token, ReferenceKind::References)?;
        }
        Ok(())
    }

    fn capture_receiver_call(&mut self, receiver: Node<'_>) -> Result<(), ExtractError> {
        let Some(end) =
            rust_receiver_call_end(self.source.as_bytes(), receiver.start_byte(), self.limit)
        else {
            return Ok(());
        };
        let name = self.compact_path(receiver.start_byte(), end)?;
        let member = name.rsplit('.').next().ok_or(ExtractError::InvalidSpan)?;
        let resolution_name = if receiver.kind() == "self" && name.matches('.').count() == 1 {
            references::rust_self_receiver_resolution(self.builder, receiver, member)?
        } else {
            Some(references::dynamic_dispatch_resolution(
                self.builder,
                member,
            )?)
        };
        let Some(resolution_name) = resolution_name else {
            return Ok(());
        };
        let span = SourceCursor::at(self.source, receiver)?.span(receiver.start_byte(), end)?;
        self.emit(ExtractedReference {
            owner: self.builder.owners.last().cloned(),
            name,
            resolution_name: Some(resolution_name),
            kind: ReferenceKind::Calls,
            span,
        })
    }

    fn capture_format_string(
        &mut self,
        tree: Node<'_>,
        literal: Node<'_>,
    ) -> Result<(), ExtractError> {
        let Some(captures) = FormatCaptures::new(self.source, literal) else {
            return Ok(());
        };
        let named_arguments = self.named_arguments(tree)?;
        let mut cursor = SourceCursor::at(self.source, literal)?;
        for (start, end) in captures {
            let name = self
                .source
                .get(start..end)
                .ok_or(ExtractError::InvalidSpan)?;
            // A `{NAME}` that names an explicit `NAME = ..` argument is not a capture.
            if !constant_shaped(name) || named_arguments.contains(&name) {
                continue;
            }
            let span = cursor.span(start, end)?;
            self.emit(ExtractedReference {
                owner: self.builder.owners.last().cloned(),
                name: self.builder.context.copy_text(name)?,
                resolution_name: None,
                kind: ReferenceKind::References,
                span,
            })?;
        }
        Ok(())
    }

    /// Constant-shaped `NAME = value` arguments of one formatting invocation.
    fn named_arguments(&mut self, tree: Node<'_>) -> Result<Vec<&'source str>, ExtractError> {
        let mut names = Vec::new();
        let mut argument_start = false;
        for token in children(tree) {
            self.poll_token()?;
            if token.is_extra() {
                continue;
            }
            let name = token_text(self.source, token);
            if argument_start
                && token.kind() == "identifier"
                && constant_shaped(name)
                && Follower::read(self.source.as_bytes(), token.end_byte(), self.limit)
                    == Follower::Binding
            {
                names
                    .try_reserve(1)
                    .map_err(|_| ExtractError::OutputLimit)?;
                names.push(name);
            }
            argument_start = opens_argument(token.kind());
        }
        Ok(names)
    }

    fn emit_token(&mut self, token: Node<'_>, kind: ReferenceKind) -> Result<(), ExtractError> {
        let name = self.builder.context.owned_text(token)?;
        self.emit(ExtractedReference {
            owner: self.builder.owners.last().cloned(),
            name,
            resolution_name: None,
            kind,
            span: span_for(token)?,
        })
    }

    fn emit_path(
        &mut self,
        token: Node<'_>,
        end: usize,
        kind: ReferenceKind,
    ) -> Result<(), ExtractError> {
        let name = self.compact_path(token.start_byte(), end)?;
        let span = SourceCursor::at(self.source, token)?.span(token.start_byte(), end)?;
        self.emit(ExtractedReference {
            owner: self.builder.owners.last().cloned(),
            name,
            resolution_name: None,
            kind,
            span,
        })
    }

    fn emit(&mut self, reference: ExtractedReference) -> Result<(), ExtractError> {
        self.emitted = self
            .emitted
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.emitted > MAX_RUST_MACRO_REFERENCES {
            return Err(ExtractError::OutputLimit);
        }
        if self.emitted.is_multiple_of(REFERENCE_CANCELLATION_INTERVAL) {
            self.builder.context.ensure_active()?;
        }
        self.builder.emit_reference(reference)
    }

    fn poll_token(&mut self) -> Result<(), ExtractError> {
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.visited.is_multiple_of(TOKEN_CANCELLATION_INTERVAL) {
            self.builder.context.ensure_active()?;
        }
        Ok(())
    }

    /// The path or receiver chain between two bytes, without the whitespace a
    /// token tree allows between its tokens.
    fn compact_path(&self, start: usize, end: usize) -> Result<String, ExtractError> {
        let raw = self
            .source
            .get(start..end)
            .ok_or(ExtractError::InvalidSpan)?;
        self.builder
            .context
            .budget
            .ensure_string_length(raw.len())?;
        let mut name = String::new();
        name.try_reserve_exact(raw.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        name.extend(raw.chars().filter(|character| !character.is_whitespace()));
        Ok(name)
    }
}

/// Whether the next token continues a path, receiver, lifetime, metavariable, or
/// interpolation, or is a name being declared.
fn continues_previous(source: &str, cursor: &FrameCursor<'_>) -> bool {
    let Some(previous) = cursor.previous else {
        return false;
    };
    let previous_text = token_text(source, previous);
    if RUST_CONTINUATION_TOKENS.contains(&previous_text) {
        return true;
    }
    let before_previous = cursor
        .before_previous
        .map(|token| token_text(source, token));
    // `static mut NAME` and `let mut name` declare through the `mut`.
    let keyword = if previous.kind() == "mutable_specifier" {
        before_previous
    } else {
        Some(previous_text)
    };
    keyword.is_some_and(|keyword| RUST_DECLARATION_KEYWORDS.contains(&keyword))
        || (previous_text == "!" && before_previous == Some("macro_rules"))
}

fn token_text<'source>(source: &'source str, token: Node<'_>) -> &'source str {
    source
        .get(token.start_byte()..token.end_byte())
        .unwrap_or_default()
}

/// A nested token tree to scan next with its role, or `None` for an attribute
/// body or `macro_rules!` template. A group that no `name!` invokes keeps the
/// expression context of the tree around it.
fn nested_tree<'tree>(
    frame: &TokenFrame<'tree>,
    tree: Node<'tree>,
) -> Option<(Node<'tree>, TreeRole)> {
    let cursor = &frame.cursor;
    let previous = cursor.previous.map(|token| token.kind());
    let before_previous = cursor.before_previous.map(|token| token.kind());
    let attribute =
        previous == Some("#") || (previous == Some("!") && before_previous == Some("#"));
    let role = match (cursor.next_tree, frame.role) {
        _ if attribute => TreeRole::Skipped,
        (Some((start, role)), _) if start == tree.start_byte() => role,
        (_, TreeRole::Expressions(_)) => TreeRole::Expressions(None),
        _ => TreeRole::Tokens,
    };
    (role != TreeRole::Skipped).then_some((tree, role))
}

fn push_frame<'tree>(
    frames: &mut Vec<TokenFrame<'tree>>,
    tree: Node<'tree>,
    role: TreeRole,
) -> Result<(), ExtractError> {
    frames
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    frames.push(TokenFrame {
        tree,
        children: children(tree),
        role,
        cursor: FrameCursor::default(),
    });
    Ok(())
}

/// Whether a name has the `SCREAMING_SNAKE_CASE` shape Rust reserves for constants and statics.
fn constant_shaped(name: &str) -> bool {
    name.len() >= MINIMUM_CONSTANT_NAME_BYTES
        && name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn rust_receiver_call_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if start >= limit || !bytes.get(start).copied().is_some_and(rust_identifier_start) {
        return None;
    }
    let mut cursor = rust_identifier_end(bytes, start, limit);
    let mut components = 0_usize;
    loop {
        let separator = skip_ascii_whitespace(bytes, cursor, limit);
        if separator >= limit || bytes.get(separator) != Some(&b'.') {
            break;
        }
        let component = skip_ascii_whitespace(bytes, separator.saturating_add(1), limit);
        if component >= limit
            || !bytes
                .get(component)
                .copied()
                .is_some_and(rust_identifier_start)
        {
            break;
        }
        cursor = rust_identifier_end(bytes, component, limit);
        components = components.checked_add(1)?;
    }
    if components == 0 {
        return None;
    }
    let call = skip_ascii_whitespace(bytes, cursor, limit);
    (call < limit && bytes.get(call) == Some(&b'(')).then_some(cursor)
}

/// End of one path component, including a raw `r#name`, or `None` when none starts here.
fn rust_component_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    let name_start = if bytes.get(start..start.saturating_add(2)) == Some(b"r#") {
        start.saturating_add(2)
    } else {
        start
    };
    (name_start < limit
        && bytes
            .get(name_start)
            .copied()
            .is_some_and(rust_identifier_start))
    .then(|| rust_identifier_end(bytes, name_start, limit))
}

fn rust_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn rust_identifier_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn rust_identifier_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    let mut cursor = start.saturating_add(1);
    while cursor < limit
        && bytes
            .get(cursor)
            .copied()
            .is_some_and(rust_identifier_continue)
    {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

fn skip_ascii_whitespace(bytes: &[u8], start: usize, limit: usize) -> usize {
    let mut cursor = start;
    while cursor < limit && bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

/// A half-open source byte range.
type ByteRange = (usize, usize);

/// Implicit argument captures in one format-string literal: the `name` of
/// `{name}` / `{name:spec}` and every `name$` count parameter, as source byte
/// ranges. `{{`/`}}` escapes, positional `{0}`/`{}` arguments, and the braces of
/// `\u{..}` escapes are not captures.
struct FormatCaptures<'text> {
    bytes: &'text [u8],
    cursor: usize,
    end: usize,
    escapes: bool,
    /// The unread format spec of the placeholder whose argument was returned last.
    spec: Option<ByteRange>,
}

/// One placeholder body split into its identifier argument and its format spec.
struct Placeholder {
    argument: Option<ByteRange>,
    spec: Option<ByteRange>,
}

impl<'text> FormatCaptures<'text> {
    /// Captures of a plain or raw string literal; byte and C strings are not format strings.
    fn new(source: &'text str, literal: Node<'_>) -> Option<Self> {
        let start = literal.start_byte();
        let end = literal.end_byte();
        let text = source.get(start..end)?;
        let (prefix, suffix, escapes) = if literal.kind() == "string_literal" {
            if !text.starts_with('"') {
                return None;
            }
            (1, 1, true)
        } else {
            // `r##"..."##`: the content sits between the quote after the hashes
            // and the quote before the same number of closing hashes.
            let hashes = text
                .strip_prefix('r')?
                .bytes()
                .take_while(|byte| *byte == b'#')
                .count();
            let quote = hashes.checked_add(1)?;
            if text.as_bytes().get(quote) != Some(&b'"') {
                return None;
            }
            (quote.checked_add(1)?, quote, false)
        };
        let content_start = start.checked_add(prefix)?;
        let content_end = end.checked_sub(suffix)?;
        (content_start <= content_end).then_some(Self {
            bytes: source.as_bytes(),
            cursor: content_start,
            end: content_end,
            escapes,
            spec: None,
        })
    }

    fn next_placeholder(&mut self) -> Option<Placeholder> {
        while self.cursor < self.end {
            let byte = self.bytes.get(self.cursor).copied()?;
            let next = self.cursor.saturating_add(1);
            if self.escapes && byte == b'\\' {
                self.cursor = escape_end(self.bytes, self.cursor, self.end);
            } else if matches!(byte, b'{' | b'}') && self.bytes.get(next) == Some(&byte) {
                self.cursor = next.saturating_add(1);
            } else if byte == b'{'
                && let Some(close) = self.placeholder_close(next)
            {
                self.cursor = close.saturating_add(1);
                return Some(placeholder_parts(self.bytes, next, close));
            } else {
                self.cursor = next;
            }
        }
        None
    }

    /// The `}` closing a placeholder body, or `None` when the body is not a placeholder.
    fn placeholder_close(&self, from: usize) -> Option<usize> {
        let mut cursor = from;
        while cursor < self.end {
            match self.bytes.get(cursor).copied()? {
                b'}' => return Some(cursor),
                b'{' => return None,
                b'\\' if self.escapes => return None,
                _ => cursor = cursor.saturating_add(1),
            }
        }
        None
    }

    /// The next `name$` count parameter in the current placeholder's spec.
    fn next_count_parameter(&mut self) -> Option<ByteRange> {
        let (mut cursor, end) = self.spec?;
        while cursor < end {
            if !self
                .bytes
                .get(cursor)
                .copied()
                .is_some_and(rust_identifier_continue)
            {
                cursor = cursor.saturating_add(1);
                continue;
            }
            let start = cursor;
            cursor = rust_identifier_end(self.bytes, start, end);
            if self
                .bytes
                .get(start)
                .copied()
                .is_some_and(rust_identifier_start)
                && cursor < end
                && self.bytes.get(cursor) == Some(&b'$')
            {
                self.spec = Some((cursor, end));
                return Some((start, cursor));
            }
        }
        self.spec = None;
        None
    }
}

impl Iterator for FormatCaptures<'_> {
    type Item = ByteRange;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(parameter) = self.next_count_parameter() {
                return Some(parameter);
            }
            let placeholder = self.next_placeholder()?;
            self.spec = placeholder.spec;
            if placeholder.argument.is_some() {
                return placeholder.argument;
            }
        }
    }
}

/// The identifier argument and the spec of the placeholder body before `close`.
fn placeholder_parts(bytes: &[u8], start: usize, close: usize) -> Placeholder {
    let colon = bytes
        .get(start..close)
        .and_then(|body| body.iter().position(|byte| *byte == b':'))
        .map_or(close, |offset| start.saturating_add(offset));
    let mut argument_end = colon;
    while argument_end > start
        && bytes
            .get(argument_end.saturating_sub(1))
            .is_some_and(u8::is_ascii_whitespace)
    {
        argument_end = argument_end.saturating_sub(1);
    }
    let identifier = argument_end > start
        && bytes.get(start).copied().is_some_and(rust_identifier_start)
        && bytes
            .get(start..argument_end)
            .is_some_and(|argument| argument.iter().copied().all(rust_identifier_continue));
    Placeholder {
        argument: identifier.then_some((start, argument_end)),
        spec: (colon < close).then(|| (colon.saturating_add(1), close)),
    }
}

/// The byte after one backslash escape, including a whole `\u{..}` code point.
fn escape_end(bytes: &[u8], backslash: usize, end: usize) -> usize {
    let after = backslash.saturating_add(2);
    if bytes.get(backslash.saturating_add(1)) == Some(&b'u') && bytes.get(after) == Some(&b'{') {
        let close = bytes
            .get(after..end)
            .and_then(|digits| digits.iter().position(|byte| *byte == b'}'));
        if let Some(offset) = close {
            return after.saturating_add(offset).saturating_add(1);
        }
    }
    after.min(end)
}

/// Monotonic byte-to-position cursor; lines are one-based and columns count bytes,
/// matching the parser's spans.
struct SourceCursor<'source> {
    bytes: &'source [u8],
    byte: usize,
    line: u32,
    column: u32,
}

impl<'source> SourceCursor<'source> {
    fn at(source: &'source str, node: Node<'_>) -> Result<Self, ExtractError> {
        let point = node.start_position();
        Ok(Self {
            bytes: source.as_bytes(),
            byte: node.start_byte(),
            line: u32::try_from(point.row)
                .ok()
                .and_then(|line| line.checked_add(1))
                .ok_or(ExtractError::InvalidSpan)?,
            column: u32::try_from(point.column).map_err(|_| ExtractError::InvalidSpan)?,
        })
    }

    fn span(&mut self, start: usize, end: usize) -> Result<SourceSpan, ExtractError> {
        let start = self.position(start)?;
        let end = self.position(end)?;
        SourceSpan::new(start, end).map_err(|_| ExtractError::InvalidSpan)
    }

    fn position(&mut self, byte: usize) -> Result<SourcePosition, ExtractError> {
        let skipped = self
            .bytes
            .get(self.byte..byte)
            .ok_or(ExtractError::InvalidSpan)?;
        for value in skipped {
            if *value == b'\n' {
                self.line = self.line.checked_add(1).ok_or(ExtractError::InvalidSpan)?;
                self.column = 0;
            } else {
                self.column = self
                    .column
                    .checked_add(1)
                    .ok_or(ExtractError::InvalidSpan)?;
            }
        }
        self.byte = byte;
        SourcePosition::new(
            u64::try_from(byte).map_err(|_| ExtractError::InvalidSpan)?,
            self.line,
            self.column,
        )
        .map_err(|_| ExtractError::InvalidSpan)
    }
}
