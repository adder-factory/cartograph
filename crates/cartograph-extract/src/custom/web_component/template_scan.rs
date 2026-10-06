//! Lexically aware scanning of component template text.
//!
//! A template expression ends at its closing delimiter outside string
//! literals, template-literal text, comments, regular-expression literals, and
//! nested braces; a tag ends at the first `>` outside quoted values and braced
//! (Svelte) values. When that structure never closes (an apostrophe in prose,
//! an unterminated comment), the first plain delimiter keeps the rest of the
//! template scannable.
//!
//! The work is linear in the file size. Structured scanning and the plain
//! fallback each draw on one per-file byte budget; a search that would examine
//! more bytes than the budget has left uses the budget up, so a run of
//! unterminated expressions ends structured scanning instead of repeating it.
//! An opener after the file's last closing delimiter is answered at once.
//! Once the structured budget is gone, tags still end at their first `>`
//! outside quotes, and the host reports the template as partial.

use crate::{ExtractError, framework::consume_quoted_byte};

use super::super::{is_identifier_body, poll_cancellation};

/// Bytes after which a `/` begins a regular expression rather than a division:
/// an operand never ends with an operator or opening punctuation.
const REGEX_PRECEDING_BYTES: &[u8] = b"(,=:[!&|?{};+-*%<>~^";
/// Keywords after which a `/` begins a regular expression: each is followed
/// by an operand, never by an operator.
const REGEX_PRECEDING_KEYWORDS: &[&[u8]] = &[
    b"await",
    b"case",
    b"delete",
    b"do",
    b"else",
    b"in",
    b"instanceof",
    b"of",
    b"return",
    b"throw",
    b"typeof",
    b"void",
    b"yield",
];
/// Length of the longest keyword in [`REGEX_PRECEDING_KEYWORDS`].
const LONGEST_REGEX_PRECEDING_KEYWORD: usize = longest_word(REGEX_PRECEDING_KEYWORDS);
/// Bytes of a comment opener (`/*` or `//`).
const COMMENT_OPENER_BYTES: usize = 2;
/// Template-literal substitutions (`${ … }`) one structured scan keeps open;
/// deeper nesting gives the expression to the plain search.
const MAX_OPEN_SUBSTITUTIONS: usize = 64;
/// Scan bytes allowed per source byte, plus a fixed allowance; the structured
/// scan and the plain fallback each get this budget.
const SCAN_BUDGET_PER_SOURCE_BYTE: usize = 4;
const SCAN_BUDGET_ALLOWANCE: usize = 64 * 1024;
/// Closing delimiter of a braced attribute value.
const BRACE_CLOSE: &str = "}";

/// Template expression delimiters of a component language.
#[derive(Clone, Copy)]
pub(super) struct TemplateDelimiters {
    pub(super) open: &'static str,
    pub(super) close: &'static str,
}

/// Vue interpolation: `{{ expression }}`.
pub(super) const VUE_DELIMITERS: TemplateDelimiters = TemplateDelimiters {
    open: "{{",
    close: "}}",
};

/// Svelte expression: `{ expression }`.
pub(super) const SVELTE_DELIMITERS: TemplateDelimiters = TemplateDelimiters {
    open: "{",
    close: "}",
};

/// Which closing delimiter a scan looks for.
#[derive(Clone, Copy)]
enum Closer {
    /// The language's template expression close (`}}` or `}`).
    Expression,
    /// The `}` ending a braced attribute value.
    Brace,
}

/// Budgeted lexical scanner over one component source.
pub(super) struct TemplateScanner<'source, 'cancel> {
    source: &'source str,
    cancelled: &'cancel mut dyn FnMut() -> bool,
    delimiters: TemplateDelimiters,
    /// Structured-scan bytes still allowed.
    remaining: usize,
    /// The structure-blind fallback search.
    plain: PlainSearch,
}

/// A quoted or braced attribute value, excluding its delimiters.
pub(super) struct TemplateAttribute<'source> {
    pub(super) start: usize,
    pub(super) value: &'source str,
    pub(super) braced: bool,
}

impl<'source, 'cancel> TemplateScanner<'source, 'cancel> {
    /// A scanner for `source` with the language's template delimiters.
    pub(super) fn new(
        source: &'source str,
        delimiters: TemplateDelimiters,
        cancelled: &'cancel mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        let last_expression_close =
            last_delimiter(source.as_bytes(), delimiters.close.as_bytes(), cancelled)?;
        let last_brace_close = if delimiters.close == BRACE_CLOSE {
            last_expression_close
        } else {
            last_delimiter(source.as_bytes(), BRACE_CLOSE.as_bytes(), cancelled)?
        };
        Ok(Self {
            source,
            cancelled,
            delimiters,
            remaining: scan_budget(source.len()),
            plain: PlainSearch {
                remaining: scan_budget(source.len()),
                last_expression_close,
                last_brace_close,
            },
        })
    }

    /// The scanned source.
    pub(super) const fn source(&self) -> &'source str {
        self.source
    }

    /// The language's template delimiters.
    pub(super) const fn delimiters(&self) -> TemplateDelimiters {
        self.delimiters
    }

    /// Offset of the delimiter closing the template expression whose content
    /// starts at `content_start`.
    pub(super) fn expression_close(
        &mut self,
        content_start: usize,
    ) -> Result<Option<usize>, ExtractError> {
        self.closing(content_start, Closer::Expression)
    }

    /// Offset after the template expression opening at `start`, or after the
    /// brace when it opens no expression.
    pub(super) fn past_expression(&mut self, start: usize) -> Result<usize, ExtractError> {
        let TemplateDelimiters { open, close } = self.delimiters;
        if !self.source[start..].starts_with(open) {
            return Ok(start + 1);
        }
        Ok(self
            .closing(start + open.len(), Closer::Expression)?
            .map_or(start + 1, |end| end + close.len()))
    }

    /// Whether unterminated template structure used up a scan budget, so
    /// later expressions and tags were found without lexical structure.
    pub(super) const fn budget_exhausted(&self) -> bool {
        self.remaining == 0 || self.plain.remaining == 0
    }

    /// Offset of the `>` ending the tag whose name starts at `from`, outside
    /// quoted and braced attribute values. Once the structured budget is used
    /// up, the first `>` outside quoted values ends the tag, as in plain HTML.
    pub(super) fn tag_end(&mut self, from: usize) -> Result<Option<usize>, ExtractError> {
        let structured = self.structured_tag_end(from)?;
        if structured.is_some() || self.remaining > 0 {
            return Ok(structured);
        }
        self.plain_tag_end(from)
    }

    /// The structured tag scan: quoted values skipped, braced values scanned
    /// as expressions, every byte charged to the structured budget.
    fn structured_tag_end(&mut self, from: usize) -> Result<Option<usize>, ExtractError> {
        let bytes = self.source.as_bytes();
        let mut index = from;
        let mut next_poll = 0;
        while let Some(&byte) = bytes.get(index) {
            poll_cancellation(self.cancelled, index, &mut next_poll)?;
            if spend_budget(&mut self.remaining, 1).is_none() {
                return Ok(None);
            }
            let end = match byte {
                b'>' => return Ok(Some(index)),
                b'"' | b'\'' => self.quote_end(index + 1, byte)?,
                b'{' => self.closing(index + 1, Closer::Brace)?,
                _ => Some(index),
            };
            let Some(end) = end else {
                return Ok(None);
            };
            index = end + 1;
        }
        Ok(None)
    }

    /// Offset of the `quote` closing an attribute value whose text starts at
    /// `from`, searched only as far as the remaining structured budget reaches.
    fn quote_end(&mut self, from: usize, quote: u8) -> Result<Option<usize>, ExtractError> {
        let bytes = self.source.as_bytes();
        let Some(window) = budget_window(bytes, from, self.remaining) else {
            return Ok(None);
        };
        let relative = find_position(window.iter(), |&byte| byte == quote, self.cancelled)?;
        if spend_budget(
            &mut self.remaining,
            relative.map_or(window.len(), |relative| relative + 1),
        )
        .is_none()
        {
            return Ok(None);
        }
        Ok(relative.map(|relative| from + relative))
    }

    fn closing(&mut self, start: usize, closer: Closer) -> Result<Option<usize>, ExtractError> {
        let (close, last) = match closer {
            Closer::Expression => (self.delimiters.close, self.plain.last_expression_close),
            Closer::Brace => (BRACE_CLOSE, self.plain.last_brace_close),
        };
        // No delimiter begins at or after `start`, so neither scan can find
        // one; answering at once keeps a run of unterminated openers linear.
        if last.is_none_or(|last| last < start) {
            return Ok(None);
        }
        let bytes = self.source.as_bytes();
        match DelimiterScan::new(bytes, close.as_bytes(), &mut self.remaining)
            .find(start, self.cancelled)?
        {
            Some(end) => Ok(Some(end)),
            None => self.plain_find(start, close.as_bytes()),
        }
    }

    /// Cancellable searches used by the layout, including long raw text.
    pub(super) fn find_from(
        &mut self,
        start: usize,
        needle: &[u8],
    ) -> Result<Option<usize>, ExtractError> {
        let found = find_position(
            self.source.as_bytes()[start..].windows(needle.len()),
            |window| window == needle,
            self.cancelled,
        )?;
        Ok(found.map(|relative| start + relative))
    }

    pub(super) fn find_case_insensitive(
        &mut self,
        start: usize,
        needle: &str,
    ) -> Result<Option<usize>, ExtractError> {
        let found = find_position(
            self.source.as_bytes()[start..].windows(needle.len()),
            |window| window.eq_ignore_ascii_case(needle.as_bytes()),
            self.cancelled,
        )?;
        Ok(found.map(|relative| start + relative))
    }

    pub(super) fn markup_start(&mut self, start: usize) -> Result<Option<usize>, ExtractError> {
        let found = find_position(
            self.source.as_bytes()[start..].iter(),
            |byte| matches!(byte, b'<' | b'{'),
            self.cancelled,
        )?;
        Ok(found.map(|relative| start + relative))
    }

    pub(super) fn is_directive(&mut self, expression: &str) -> Result<bool, ExtractError> {
        is_template_directive(expression, self.cancelled)
    }

    /// Attribute lookup outside all other quoted and braced values. A damaged
    /// braced value abstains instead of using the structure-blind fallback.
    pub(super) fn attribute(
        &mut self,
        key: &str,
    ) -> Result<Option<TemplateAttribute<'source>>, ExtractError> {
        let bytes = self.source.as_bytes();
        let mut index = 0;
        let mut next_poll = 0;
        while let Some(&byte) = bytes.get(index) {
            poll_cancellation(self.cancelled, index, &mut next_poll)?;
            let preceded = index > 0 && bytes[index - 1].is_ascii_whitespace();
            if preceded
                && bytes
                    .get(index..index + key.len())
                    .is_some_and(|name| name.eq_ignore_ascii_case(key.as_bytes()))
                && let Some(value) = self.attribute_at(index + key.len())?
            {
                return Ok(Some(value));
            }
            let end = match byte {
                b'"' | b'\'' => self.quote_end(index + 1, byte)?,
                b'{' => DelimiterScan::new(bytes, BRACE_CLOSE.as_bytes(), &mut self.remaining)
                    .find(index + 1, self.cancelled)?,
                _ => Some(index),
            };
            let Some(end) = end else {
                return Ok(None);
            };
            index = end + 1;
        }
        Ok(None)
    }

    fn attribute_at(
        &mut self,
        after_key: usize,
    ) -> Result<Option<TemplateAttribute<'source>>, ExtractError> {
        let equals = past_whitespace(self.source, after_key, self.cancelled)?;
        if self.source.as_bytes().get(equals) != Some(&b'=') {
            return Ok(None);
        }
        let open = past_whitespace(self.source, equals + 1, self.cancelled)?;
        let bytes = self.source.as_bytes();
        let braced = bytes.get(open) == Some(&b'{');
        let end = match bytes.get(open) {
            Some(b'"' | b'\'') => self.quote_end(open + 1, bytes[open])?,
            Some(b'{') => DelimiterScan::new(bytes, BRACE_CLOSE.as_bytes(), &mut self.remaining)
                .find(open + 1, self.cancelled)?,
            _ => return Ok(None),
        };
        Ok(end.map(|end| TemplateAttribute {
            start: open + 1,
            value: &self.source[open + 1..end],
            braced,
        }))
    }

    fn plain_find(&mut self, start: usize, close: &[u8]) -> Result<Option<usize>, ExtractError> {
        let Some(window) = budget_window(self.source.as_bytes(), start, self.plain.remaining)
        else {
            return Ok(None);
        };
        let found = find_position(
            window.windows(close.len()),
            |candidate| candidate == close,
            self.cancelled,
        )?;
        let examined = found.map_or(window.len(), |relative| relative + close.len());
        self.plain.remaining = self.plain.remaining.saturating_sub(examined);
        Ok(found.map(|relative| start + relative))
    }

    fn plain_tag_end(&mut self, start: usize) -> Result<Option<usize>, ExtractError> {
        let Some(window) = budget_window(self.source.as_bytes(), start, self.plain.remaining)
        else {
            return Ok(None);
        };
        let mut quote = None;
        let found = find_position(
            window.iter(),
            |&byte| match quote {
                Some(open) => {
                    if byte == open {
                        quote = None;
                    }
                    false
                }
                None if matches!(byte, b'"' | b'\'') => {
                    quote = Some(byte);
                    false
                }
                None => byte == b'>',
            },
            self.cancelled,
        )?;
        let examined = found.map_or(window.len(), |relative| relative + 1);
        self.plain.remaining = self.plain.remaining.saturating_sub(examined);
        Ok(found.map(|relative| start + relative))
    }
}

/// Offset of the first non-whitespace byte at or after `start` (the source
/// length when there is none).
fn past_whitespace(
    source: &str,
    start: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<usize, ExtractError> {
    let found = find_position(
        source.as_bytes()[start..].iter(),
        |byte| !byte.is_ascii_whitespace(),
        cancelled,
    )?;
    Ok(found.map_or(source.len(), |relative| start + relative))
}

/// Search a bounded byte iterator, polling at the custom scanner's byte cadence.
fn find_position<Item>(
    items: impl IntoIterator<Item = Item>,
    mut matches: impl FnMut(Item) -> bool,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<usize>, ExtractError> {
    let mut next_poll = 0;
    for (index, item) in items.into_iter().enumerate() {
        poll_cancellation(cancelled, index, &mut next_poll)?;
        if matches(item) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn last_delimiter(
    bytes: &[u8],
    close: &[u8],
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<usize>, ExtractError> {
    let found = find_position(
        bytes.windows(close.len()).rev(),
        |window| window == close,
        cancelled,
    )?;
    Ok(found.map(|relative| bytes.len() - close.len() - relative))
}

fn budget_window(bytes: &[u8], start: usize, remaining: usize) -> Option<&[u8]> {
    bytes.get(start..bytes.len().min(start.saturating_add(remaining)))
}

/// Charge `bytes` to `remaining`. A charge larger than what is left uses the
/// budget up: the search already examined those bytes, and an unchanged budget
/// would let the next opener repeat the same work.
fn spend_budget(remaining: &mut usize, bytes: usize) -> Option<()> {
    if let Some(left) = remaining.checked_sub(bytes) {
        *remaining = left;
        Some(())
    } else {
        *remaining = 0;
        None
    }
}

/// Scan bytes allowed for a source of `source_len` bytes.
const fn scan_budget(source_len: usize) -> usize {
    source_len
        .saturating_mul(SCAN_BUDGET_PER_SOURCE_BYTE)
        .saturating_add(SCAN_BUDGET_ALLOWANCE)
}

/// The structure-blind delimiter search used when a structured scan fails.
struct PlainSearch {
    /// Bytes the plain search may still examine.
    remaining: usize,
    /// Start of the file's last expression close delimiter.
    last_expression_close: Option<usize>,
    /// Start of the file's last `}`.
    last_brace_close: Option<usize>,
}

/// Whether a template expression is a block or directive tag (`{#if}`,
/// `{/if}`, `{:else}`, `{@html}`) rather than an expression. A leading `/` is a
/// closing tag unless it opens a comment or a regular-expression literal.
fn is_template_directive(
    expression: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    let bytes = expression.as_bytes();
    Ok(match bytes.first() {
        Some(b'#' | b':' | b'@') => true,
        Some(b'/') => {
            !matches!(bytes.get(1), Some(b'*' | b'/'))
                && matches!(
                    regex_literal_end(bytes, 1, cancelled)?,
                    RegexEnd::Unterminated(_)
                )
        }
        _ => false,
    })
}

/// What a `/` began.
enum Slash {
    /// A comment ending at the given offset; the preceding token stays current.
    Comment(usize),
    /// A regular-expression literal or a division ending at the given offset.
    Token(usize),
}

/// String-literal state of a structured scan, including the template-literal
/// substitutions (`${ … }`) it is inside.
#[derive(Default)]
struct Quoting {
    quote: Option<u8>,
    escaped: bool,
    /// Brace depth at which each open substitution returns to its literal.
    substitutions: Vec<usize>,
}

/// What the previous code token is, as far as classifying a `/` needs.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum TokenShape {
    /// Punctuation, an operator, or a string literal's closing quote.
    #[default]
    Other,
    /// A `.` that accesses a member: not part of `...`, and not the decimal
    /// point of a number it directly follows.
    MemberDot,
    /// A word that began right after a member `.`, across whitespace and
    /// comments (`value . /* c */ in`): a property name, never a keyword.
    Property,
    /// A word that began with a digit.
    Number,
    /// Any other word: an identifier or a keyword.
    Word,
}

/// The last code byte before the scan position that is not whitespace or
/// part of a comment, and the shape of the token it ends.
#[derive(Clone, Copy, Default)]
struct PreviousToken {
    byte: Option<u8>,
    at: usize,
    shape: TokenShape,
}

impl PreviousToken {
    /// A code byte that is not part of a word.
    const fn code(byte: u8, at: usize) -> Self {
        Self {
            byte: Some(byte),
            at,
            shape: TokenShape::Other,
        }
    }

    /// The state after the code byte at `at`.
    fn advanced(self, bytes: &[u8], at: usize) -> Self {
        let byte = bytes.get(at).copied();
        let shape = match byte {
            Some(byte) if is_identifier_body(byte) => self.word_shape(byte, at),
            Some(b'.') if self.is_member_dot(bytes, at) => TokenShape::MemberDot,
            _ => TokenShape::Other,
        };
        Self { byte, at, shape }
    }

    /// The shape of the word the identifier byte `byte` at `at` begins or continues.
    fn word_shape(self, byte: u8, at: usize) -> TokenShape {
        let in_word = matches!(
            self.shape,
            TokenShape::Property | TokenShape::Number | TokenShape::Word
        );
        if in_word && self.at + 1 == at {
            self.shape
        } else if self.shape == TokenShape::MemberDot {
            TokenShape::Property
        } else if byte.is_ascii_digit() {
            TokenShape::Number
        } else {
            TokenShape::Word
        }
    }

    /// Whether the `.` at `at` accesses a member rather than belonging to a
    /// spread (`...`) or a number (`1.`).
    fn is_member_dot(self, bytes: &[u8], at: usize) -> bool {
        let spread = at.checked_sub(1).and_then(|before| bytes.get(before)) == Some(&b'.')
            || bytes.get(at + 1) == Some(&b'.');
        let decimal = self.shape == TokenShape::Number && self.at + 1 == at;
        !spread && !decimal
    }

    /// Whether the token is a postfix `++` or `--`, after which `/` divides.
    fn is_update_operator(self, bytes: &[u8]) -> bool {
        self.byte.is_some_and(|byte| {
            matches!(byte, b'+' | b'-')
                && self.at.checked_sub(1).and_then(|before| bytes.get(before)) == Some(&byte)
        })
    }
}

/// Lexical state of one structured expression scan.
struct DelimiterScan<'scan> {
    bytes: &'scan [u8],
    close: &'scan [u8],
    remaining: &'scan mut usize,
    depth: usize,
    quoting: Quoting,
    previous: PreviousToken,
    /// A line already scanned without a closing `/` holds no regex literal.
    regex_free_until: usize,
}

impl<'scan> DelimiterScan<'scan> {
    fn new(bytes: &'scan [u8], close: &'scan [u8], remaining: &'scan mut usize) -> Self {
        Self {
            bytes,
            close,
            remaining,
            depth: 0,
            quoting: Quoting::default(),
            previous: PreviousToken::default(),
            regex_free_until: 0,
        }
    }

    fn find(
        mut self,
        start: usize,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Option<usize>, ExtractError> {
        let mut index = start;
        let mut next_poll = 0;
        while let Some(&byte) = self.bytes.get(index) {
            poll_cancellation(cancelled, index, &mut next_poll)?;
            if self.spend(1).is_none() {
                return Ok(None);
            }
            if self.quoting.quote.is_some() {
                match self.past_quoted_byte(byte, index) {
                    Some(next) => index = next,
                    None => return Ok(None),
                }
                continue;
            }
            if byte == b'/' {
                match self.past_slash(index, cancelled)? {
                    Some(Slash::Comment(end)) => {
                        index = end + 1;
                        continue;
                    }
                    Some(Slash::Token(end)) => index = end,
                    None => return Ok(None),
                }
            } else if self.is_close(byte, index) {
                return Ok(Some(index));
            }
            self.advance_previous_token(byte, index);
            index += 1;
        }
        Ok(None)
    }

    fn advance_previous_token(&mut self, byte: u8, index: usize) {
        if !byte.is_ascii_whitespace() {
            self.previous = self.previous.advanced(self.bytes, index);
        }
    }

    fn spend(&mut self, bytes: usize) -> Option<()> {
        spend_budget(self.remaining, bytes)
    }

    /// Consume one byte of a string literal and return the next offset. A
    /// `${` in template-literal text opens a substitution scanned as code.
    fn past_quoted_byte(&mut self, byte: u8, index: usize) -> Option<usize> {
        let opens_substitution = self.quoting.quote == Some(b'`')
            && !self.quoting.escaped
            && byte == b'$'
            && self.bytes.get(index + 1) == Some(&b'{');
        if opens_substitution {
            if self.quoting.substitutions.len() >= MAX_OPEN_SUBSTITUTIONS {
                return None;
            }
            self.spend(1)?;
            self.quoting.substitutions.push(self.depth);
            self.quoting.quote = None;
            self.depth = self.depth.saturating_add(1);
            self.previous = PreviousToken::code(b'{', index + 1);
            return Some(index + 2);
        }
        consume_quoted_byte(byte, &mut self.quoting.quote, &mut self.quoting.escaped);
        if self.quoting.quote.is_none() {
            // A closed literal is an operand: a `/` after it divides.
            self.previous = PreviousToken::code(byte, index);
        }
        Some(index + 1)
    }

    /// End of the bytes the remaining budget may scan from `from`.
    fn window_end(&self, from: usize) -> usize {
        self.bytes.len().min(from.saturating_add(*self.remaining))
    }

    /// Classify the `/` at `index` and find where its comment or token ends.
    fn past_slash(
        &mut self,
        index: usize,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Option<Slash>, ExtractError> {
        let terminator: &[u8] = match self.bytes.get(index + 1) {
            Some(b'*') => b"*/",
            Some(b'/') => b"\n",
            _ => return self.past_regex_or_division(index, cancelled),
        };
        let from = index + COMMENT_OPENER_BYTES;
        let Some(window) = self.bytes.get(from..self.window_end(from)) else {
            return Ok(None);
        };
        let Some(relative) = find_position(
            window.windows(terminator.len()),
            |candidate| candidate == terminator,
            cancelled,
        )?
        else {
            self.spend(window.len());
            return Ok(None);
        };
        let end = from + relative + terminator.len() - 1;
        Ok(self.spend(end - index).map(|()| Slash::Comment(end)))
    }

    fn past_regex_or_division(
        &mut self,
        index: usize,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Option<Slash>, ExtractError> {
        if index < self.regex_free_until || !self.regex_may_follow() {
            return Ok(Some(Slash::Token(index)));
        }
        let Some(window) = self.bytes.get(..self.window_end(index + 1)) else {
            return Ok(None);
        };
        Ok(match regex_literal_end(window, index + 1, cancelled)? {
            RegexEnd::Closed(end) => self.spend(end - index).map(|()| Slash::Token(end)),
            RegexEnd::Unterminated(line_end) => {
                self.regex_free_until = line_end;
                self.spend(line_end.saturating_sub(index))
                    .map(|()| Slash::Token(index))
            }
        })
    }

    /// Whether a `/` after the previous token begins a regular expression: an
    /// operand never ends with an operator, opening punctuation, or a keyword
    /// that expects an operand. A postfix `++`/`--` and a property name such
    /// as `value.in` end an operand.
    fn regex_may_follow(&self) -> bool {
        match self.previous.byte {
            None => true,
            Some(_) if self.previous.is_update_operator(self.bytes) => false,
            Some(byte) if REGEX_PRECEDING_BYTES.contains(&byte) => true,
            Some(byte) if byte.is_ascii_alphabetic() => {
                self.previous.shape != TokenShape::Property
                    && ends_with_regex_keyword(self.bytes, self.previous.at)
            }
            Some(_) => false,
        }
    }

    /// Track quotes and braces; whether the delimiter closes the expression here.
    fn is_close(&mut self, byte: u8, index: usize) -> bool {
        match byte {
            b'\'' | b'"' | b'`' => self.quoting.quote = Some(byte),
            b'{' => self.depth = self.depth.saturating_add(1),
            b'}' if self.depth > 0 => {
                self.depth -= 1;
                if self.quoting.substitutions.last() == Some(&self.depth) {
                    self.quoting.substitutions.pop();
                    self.quoting.quote = Some(b'`');
                }
            }
            _ => return self.depth == 0 && self.bytes[index..].starts_with(self.close),
        }
        false
    }
}

/// Whether the identifier ending at `last` is a keyword that expects an
/// operand.
fn ends_with_regex_keyword(bytes: &[u8], last: usize) -> bool {
    let end = last.saturating_add(1);
    let floor = end.saturating_sub(LONGEST_REGEX_PRECEDING_KEYWORD + 1);
    let Some(window) = bytes.get(floor..end) else {
        return false;
    };
    let word = match window.iter().rposition(|&byte| !is_identifier_body(byte)) {
        Some(boundary) => &window[boundary + 1..],
        None if floor == 0 => window,
        // Longer than every keyword.
        None => return false,
    };
    REGEX_PRECEDING_KEYWORDS.contains(&word)
}

/// Length of the longest word in `words`.
const fn longest_word(words: &[&[u8]]) -> usize {
    let mut longest = 0;
    let mut index = 0;
    while index < words.len() {
        if words[index].len() > longest {
            longest = words[index].len();
        }
        index += 1;
    }
    longest
}

/// Offset of the closing `/` of a regular expression body starting at `from`,
/// honoring escapes and character classes, or the line end (or the end of
/// `bytes`) reached without one.
enum RegexEnd {
    Closed(usize),
    Unterminated(usize),
}

fn regex_literal_end(
    bytes: &[u8],
    from: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<RegexEnd, ExtractError> {
    let mut in_class = false;
    let mut index = from;
    let mut next_poll = 0;
    while let Some(&byte) = bytes.get(index) {
        poll_cancellation(cancelled, index, &mut next_poll)?;
        match byte {
            b'\\' => index += 1,
            b'\n' | b'\r' => return Ok(RegexEnd::Unterminated(index)),
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => return Ok(RegexEnd::Closed(index)),
            _ => {}
        }
        index += 1;
    }
    Ok(RegexEnd::Unterminated(bytes.len()))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{
        super::layout::{TemplateSurface, component_layout, template_expression_regions},
        SVELTE_DELIMITERS, TemplateDelimiters, TemplateScanner, VUE_DELIMITERS, scan_budget,
    };
    use crate::{SourceLimits, SourceSnapshot, walk::embedded_script::ScriptDialect};

    /// Source limit of the pathological fixtures.
    const LIMIT: usize = 4 * 1024 * 1024;
    /// Openers in each pathological fixture.
    const OPENERS: usize = 100_000;
    /// Bytes the layout and template-expression scans of one component examined.
    struct ScanWork {
        structured: usize,
        plain: usize,
        expressions: usize,
        tags: usize,
        exhausted: bool,
    }

    fn scan_work(path: &str, source: &str, delimiters: TemplateDelimiters) -> ScanWork {
        let limits = SourceLimits::new(LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("{path} snapshot: {error}"));
        let mut cancelled = || false;
        let mut scanner = TemplateScanner::new(snapshot.source(), delimiters, &mut cancelled)
            .unwrap_or_else(|error| panic!("{path} scanner: {error}"));
        let layout =
            component_layout(&mut scanner).unwrap_or_else(|error| panic!("{path} layout: {error}"));
        let surface = TemplateSurface {
            layout: &layout,
            dialect: ScriptDialect::JavaScript,
        };
        let expressions = template_expression_regions(&mut scanner, &surface)
            .unwrap_or_else(|error| panic!("{path} expressions: {error}"))
            .len();
        ScanWork {
            structured: scan_budget(source.len()) - scanner.remaining,
            plain: scan_budget(source.len()) - scanner.plain.remaining,
            expressions,
            tags: layout.tags.len(),
            exhausted: scanner.budget_exhausted(),
        }
    }

    fn repeated(prefix: &str, unit: &str, suffix: &str) -> String {
        let mut source = String::from(prefix);
        for _ in 0..OPENERS {
            source.push_str(unit);
        }
        source.push_str(suffix);
        source
    }

    /// Pathological templates: `(file, delimiters, prefix, unit, suffix)`, where
    /// `unit` repeats between `prefix` and `suffix`.
    type Pathological = (
        &'static str,
        TemplateDelimiters,
        &'static str,
        &'static str,
        &'static str,
    );
    const PATHOLOGICAL: &[Pathological] = &[
        ("Open.svelte", SVELTE_DELIMITERS, "<p>{ok()}</p>", "{", ""),
        (
            "Quote.svelte",
            SVELTE_DELIMITERS,
            "<p>{ok()}</p>",
            "{\"",
            "",
        ),
        (
            "Comment.svelte",
            SVELTE_DELIMITERS,
            "<p>{ok()}</p>",
            "{/*",
            "",
        ),
        ("Slash.svelte", SVELTE_DELIMITERS, "<p>{ok()}</p>", "{/", ""),
        (
            "Template.svelte",
            SVELTE_DELIMITERS,
            "<p>{ok()}</p>",
            "{`${",
            "",
        ),
        (
            "Braced.svelte",
            SVELTE_DELIMITERS,
            "<p>{ok()}</p>",
            "<a on:click={",
            "",
        ),
        ("Closed.svelte", SVELTE_DELIMITERS, "<p>", "{", "}"),
        (
            "Open.vue",
            VUE_DELIMITERS,
            "<template><p>{{ ok() }}</p>",
            "{{ a",
            "",
        ),
        (
            "Paren.vue",
            VUE_DELIMITERS,
            "<template><p>{{ ok() }}</p>",
            "{{ (",
            "",
        ),
        ("Single.vue", VUE_DELIMITERS, "<template>", "{", "}"),
        ("Closed.vue", VUE_DELIMITERS, "<template>", "{{ a", "}}"),
    ];

    /// A run of unterminated openers after the last closing delimiter costs
    /// constant work per opener, and openers before it jump past it, so both
    /// scans stay within a small multiple of the file size (they used to
    /// rescan the tail once per opener: quadratic).
    #[test]
    fn cancellation_inside_a_large_template_expression_stops_the_layout_scan() {
        let filler = "x".repeat(256 * 1024);
        for source in [
            format!("{{ /*{filler}*/ live() }}"),
            format!("{{ //{filler}\n live() }}"),
            format!("{{ '{filler}' + live() }}"),
            format!("{{ /{filler}/.test(live()) }}"),
            format!("<p title=\"{filler}\">{{live()}}</p>"),
            format!("{{ '{filler} }}"),
            format!("{{ `{filler}${{live()}}` }}"),
            format!("<!--{filler}--><p>{{live()}}</p>"),
            format!("<script>/*{filler}*/ live()</script>"),
            format!("{filler}<p>{{live()}}</p>"),
        ] {
            let limits = SourceLimits::new(LIMIT).unwrap_or_else(|error| panic!("limits: {error}"));
            let snapshot =
                SourceSnapshot::from_bytes("src/Cancel.svelte", source.as_bytes(), limits)
                    .unwrap_or_else(|error| panic!("snapshot: {error}"));
            let polls = Cell::new(0);
            let armed = Cell::new(false);
            let mut cancelled = || {
                if armed.get() {
                    polls.set(polls.get() + 1);
                }
                polls.get() == 4
            };
            let mut scanner =
                TemplateScanner::new(snapshot.source(), SVELTE_DELIMITERS, &mut cancelled)
                    .unwrap_or_else(|error| panic!("scanner: {error}"));
            armed.set(true);
            let outcome = component_layout(&mut scanner);
            assert!(matches!(outcome, Err(crate::ExtractError::Cancelled)));
        }
    }

    #[test]
    fn cancellation_in_plain_fallback_searches_is_not_a_delimiter_miss() {
        let filler = "x".repeat(256 * 1024);
        for (source, tag) in [
            (format!("{{ /*{filler}*/ live() }}"), false),
            (format!("<p title=\"{filler}\">"), true),
        ] {
            let armed = Cell::new(false);
            let polls = Cell::new(0);
            let mut cancelled = || {
                if armed.get() {
                    polls.set(polls.get() + 1);
                }
                polls.get() == 3
            };
            let mut scanner = TemplateScanner::new(&source, SVELTE_DELIMITERS, &mut cancelled)
                .unwrap_or_else(|error| panic!("scanner: {error}"));
            scanner.remaining = 0;
            armed.set(true);
            let outcome = if tag {
                scanner.tag_end(1)
            } else {
                scanner.expression_close(1)
            };
            assert_eq!(outcome, Err(crate::ExtractError::Cancelled));
        }
    }

    #[test]
    fn cancellation_interrupts_the_last_delimiter_preflight() {
        let source = "x".repeat(256 * 1024);
        let mut polls = 0;
        let mut cancelled = || {
            polls += 1;
            polls == 3
        };
        assert!(matches!(
            TemplateScanner::new(&source, SVELTE_DELIMITERS, &mut cancelled),
            Err(crate::ExtractError::Cancelled)
        ));
    }

    #[test]
    fn unterminated_openers_cost_work_linear_in_the_file() {
        for &(path, delimiters, prefix, unit, suffix) in PATHOLOGICAL {
            let source = repeated(prefix, unit, suffix);
            let work = scan_work(&format!("src/{path}"), &source, delimiters);
            assert!(
                work.plain <= 2 * source.len(),
                "{path}: plain search examined {} bytes of a {}-byte file",
                work.plain,
                source.len()
            );
            assert!(
                work.structured <= scan_budget(source.len()),
                "{path}: structured scan exceeded its budget"
            );
            assert!(
                work.expressions <= 2,
                "{path}: {} expressions",
                work.expressions
            );
        }
    }

    /// The same bounds leave a well-formed template fully scanned.
    #[test]
    fn closed_expressions_are_all_found_within_the_budget() {
        let source = repeated("<p>", "{f(`a${b}c`, /}/)}", "</p>");
        let work = scan_work("src/Closed.svelte", &source, SVELTE_DELIMITERS);
        assert_eq!(work.expressions, OPENERS);
        assert_eq!(work.plain, 0, "no structured scan fell back");
        assert!(!work.exhausted, "a well-formed template never exhausts");
    }

    /// A search cut short by the budget charges everything it examined even
    /// when that is more than is left: the budget is used up instead of
    /// staying untouched, which let every following opener repeat the same
    /// long search (quadratic work on `{a(/[x)}` repeated after a long prefix).
    #[test]
    fn a_search_cut_short_by_the_budget_uses_it_up() {
        /// Structured budget left when the scan starts.
        const LEFT: usize = 64;
        // `(/[` opens a regular-expression class that never closes.
        let source = format!("{{a(/[{}}}", "x".repeat(4 * LEFT));
        let mut cancelled = || false;
        let mut scanner = TemplateScanner::new(&source, SVELTE_DELIMITERS, &mut cancelled)
            .unwrap_or_else(|error| panic!("scanner: {error}"));
        scanner.remaining = LEFT;
        assert_eq!(
            scanner.expression_close(1),
            Ok(Some(source.len() - 1)),
            "the plain search still closes the expression"
        );
        assert_eq!(scanner.remaining, 0, "the cut-short search was not charged");
    }

    /// End to end, a run of such openers after a long prefix keeps every
    /// expression, uses the structured budget up, and keeps the plain search
    /// within the file size.
    #[test]
    fn unclosed_regex_classes_keep_every_expression_within_the_bounds() {
        let mut source = "x".repeat(OPENERS);
        source.push_str(&repeated("", "{a(/[x)}", ""));
        let work = scan_work("src/Regex.svelte", &source, SVELTE_DELIMITERS);
        assert!(work.exhausted);
        assert!(work.plain <= 2 * source.len());
        assert_eq!(work.expressions, OPENERS);
    }

    /// Once unterminated structure has used the structured budget up, later
    /// tags still end at their first unquoted `>`, so component tags and raw
    /// elements after the damage are kept.
    #[test]
    fn tags_after_an_exhausted_budget_still_end() {
        let source = repeated(
            "",
            "{/*}",
            "<script>keep()</script><Card a=\"x>y\" /><Row />",
        );
        let work = scan_work("src/Late.svelte", &source, SVELTE_DELIMITERS);
        assert!(work.exhausted);
        assert_eq!(
            work.tags, 2,
            "<Card> and <Row> end after the exhausted budget"
        );
        assert!(work.plain <= 2 * source.len());
    }
}
