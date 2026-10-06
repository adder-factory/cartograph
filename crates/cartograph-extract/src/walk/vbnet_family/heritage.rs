//! Recovery of `Inherits` / `Implements` statements the VB grammar cannot parse.
//!
//! The grammar only accepts heritage clauses on the type line itself. The
//! idiomatic form puts each clause on its own line, which the grammar recovers
//! as an `ERROR` node and/or a bogus field declaration; the `:`-separated form
//! (`Class Kid : Inherits Base : Implements I`) recovers the same way. A node
//! whose statement (its line, or the text between the `:` separators around
//! it) starts with the keyword is re-read from there as a keyword-anchored,
//! comma-separated list of dotted type names (`Acme.Base(Of T)` names
//! `Acme.Base`), or nothing when the clause is not exactly that shape.

use std::{cmp::Ordering, collections::BTreeSet, mem::size_of};

use cartograph_domain::{ReferenceKind, SourcePosition, SourceSpan, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference};

use super::ExtractionBuilder;

/// Longest physical line inspected for a heritage clause.
const MAX_HERITAGE_LINE_BYTES: usize = 1_024;
/// Most heritage targets read from one clause.
const MAX_HERITAGE_TARGETS: usize = 64;
const MAX_TARGET_BYTES: usize = 512;
/// Most ` _`-continued physical lines read for one clause.
const MAX_CONTINUATION_LINES: usize = 16;

/// Recovery nodes can repeat one clause; deduplication stays local to the file.
#[derive(Default)]
pub(in crate::walk) struct HeritageSeen(BTreeSet<HeritageKey>);

#[derive(Clone, PartialEq, Eq)]
struct HeritageKey {
    span: SourceSpan,
    kind: ReferenceKind,
    owner: Option<SymbolId>,
}

impl Ord for HeritageKey {
    fn cmp(&self, other: &Self) -> Ordering {
        #[cfg(test)]
        HERITAGE_WORK.set(HERITAGE_WORK.get().saturating_add(1));
        (self.span, self.kind, &self.owner).cmp(&(other.span, other.kind, &other.owner))
    }
}

impl PartialOrd for HeritageKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl HeritageSeen {
    fn record(
        builder: &mut ExtractionBuilder<'_, '_>,
        key: HeritageKey,
    ) -> Result<bool, ExtractError> {
        if builder.vbnet_heritage.0.contains(&key) {
            return Ok(false);
        }
        let bytes = size_of::<HeritageKey>()
            .saturating_mul(2)
            .saturating_add(64);
        builder
            .context
            .budget
            .reserve_working_bytes(u64::try_from(bytes).map_err(|_| ExtractError::OutputLimit)?)?;
        if let Some(owner) = &key.owner {
            builder
                .context
                .budget
                .reserve_additional_string(owner.as_str())?;
        }
        builder.vbnet_heritage.0.insert(key);
        Ok(true)
    }
}

/// One recovered heritage statement line.
#[derive(Clone, Copy)]
pub(super) struct HeritageLine<'source> {
    kind: ReferenceKind,
    /// Clause text after the keyword, without any trailing comment.
    clause: &'source str,
    /// Byte offset of `clause` in the source.
    clause_start: usize,
    /// Byte offset of the start of the line.
    line_start: usize,
    /// Zero-based row of the line.
    row: usize,
    /// Whether the clause provably belongs to the current owner; an unowned
    /// statement is still a heritage statement (never a field) but emits
    /// nothing.
    owned: bool,
}

/// The heritage statement on the line where `node` starts, when that line is
/// an `Inherits`/`Implements` statement directly inside a type block.
pub(super) fn heritage_line<'source>(
    builder: &ExtractionBuilder<'source, '_>,
    node: Node<'_>,
) -> Result<Option<HeritageLine<'source>>, ExtractError> {
    let in_type = matches!(
        builder.native_owner_kinds.last(),
        Some(SymbolKind::Class | SymbolKind::Interface | SymbolKind::Struct)
    );
    if !in_type {
        return Ok(None);
    }
    let source = builder.context.snapshot.source();
    // Tree-sitter columns are byte offsets from the line start, which is a
    // character boundary.
    let position = node.start_position();
    let Some(line_start) = node.start_byte().checked_sub(position.column) else {
        return Err(ExtractError::InvalidSpan);
    };
    let Some(line) = physical_line(source, line_start) else {
        return Ok(None);
    };
    let statement = statement_offset(line, position.column);
    let code = statement_code(&line[statement..]);
    let Some((kind, clause_offset)) = heritage_keyword(code) else {
        return Ok(None);
    };
    let owned = statement_owned(
        builder,
        StatementSite {
            line_start,
            statement_start: line_start + statement,
            row: position.row,
        },
    );
    Ok(Some(HeritageLine {
        kind,
        clause: &code[clause_offset..],
        clause_start: line_start + statement + clause_offset,
        line_start,
        row: position.row,
        owned,
    }))
}

/// The heritage kind of a statement starting with `Inherits`/`Implements`
/// (any case) and the offset of the clause after the keyword.
fn heritage_keyword(code: &str) -> Option<(ReferenceKind, usize)> {
    let trimmed = code.trim_start();
    let keyword_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let keyword = &trimmed[..keyword_end];
    let kind = if keyword.eq_ignore_ascii_case("inherits") {
        ReferenceKind::Extends
    } else if keyword.eq_ignore_ascii_case("implements") {
        ReferenceKind::Implements
    } else {
        return None;
    };
    Some((kind, code.len() - trimmed.len() + keyword_end))
}

/// Byte offset in `line` of the statement containing the node at `column`:
/// after the last `:` separator before the node (`Class Kid : Inherits Base`),
/// or the line start. A prefix holding a string or comment quote is never
/// split, so a `:` inside a literal cannot start a statement.
fn statement_offset(line: &str, column: usize) -> usize {
    line.get(..column)
        .filter(|prefix| !prefix.contains(['"', '\'']))
        .and_then(|prefix| prefix.rfind(':'))
        .map_or(0, |colon| colon + 1)
}

/// Statement text up to the next `:` separator or `'` comment
/// (`Inherits Base : Implements I` reads `Inherits Base`).
fn statement_code(text: &str) -> &str {
    text.split([':', '\'']).next().unwrap_or_default()
}

/// Where a `:`-separated statement starts: its physical line, its own byte
/// offset, and the zero-based row.
#[derive(Clone, Copy)]
struct StatementSite {
    line_start: usize,
    statement_start: usize,
    row: usize,
}

/// Whether a heritage statement provably belongs to the current owner.
///
/// A statement that begins its logical line (physical lines joined by ` _`
/// continuations) does. Otherwise every statement before it on the logical
/// line must be a heritage statement, except the first, which may instead be
/// the owner's own header: the owner must start on that logical line. A
/// nested header the grammar failed to parse (`Class Outer : Class Inner :
/// Inherits X`, or `Class Inner : _` continued onto the next line) therefore
/// never hands its heritage to the enclosing type. A prefix holding a quote
/// abstains.
fn statement_owned(builder: &ExtractionBuilder<'_, '_>, site: StatementSite) -> bool {
    let source = builder.context.snapshot.source();
    let (logical_start, logical_row) = logical_line_start(source, site);
    let Some(prefix) = source.get(logical_start..site.statement_start) else {
        return false;
    };
    if without_continuations(prefix).is_empty() {
        return true;
    }
    if prefix.contains(['"', '\'']) {
        return false;
    }
    let mut statements = prefix.split(':').map(without_continuations);
    let first = statements.next().unwrap_or_default();
    let later_are_heritage = statements
        .filter(|statement| !statement.is_empty())
        .all(|statement| heritage_keyword(statement).is_some());
    later_are_heritage
        && (heritage_keyword(first).is_some() || owner_starts_at_or_after(builder, logical_row))
}

/// `text` without leading whitespace and leading ` _` line-continuation
/// markers, which are whitespace to the VB lexer.
fn without_continuations(text: &str) -> &str {
    let mut rest = text.trim_start();
    while let Some(after) = rest
        .strip_prefix('_')
        .filter(|after| after.is_empty() || after.starts_with(char::is_whitespace))
    {
        rest = after.trim_start();
    }
    rest
}

/// The byte offset and row of the first physical line of the logical line
/// holding `site`, following preceding ` _` continuations (bounded).
fn logical_line_start(source: &str, site: StatementSite) -> (usize, usize) {
    let (mut start, mut row) = (site.line_start, site.row);
    for _ in 0..MAX_CONTINUATION_LINES {
        let Some(previous) = previous_line_start(source, start) else {
            break;
        };
        let continued = source
            .get(previous..start - 1)
            .and_then(|line| line.trim_end().strip_suffix('_'))
            .is_some_and(|body| body.ends_with(char::is_whitespace));
        if !continued {
            break;
        }
        (start, row) = (previous, row.saturating_sub(1));
    }
    (start, row)
}

/// Start of the physical line before the one starting at `line_start`, when
/// that line is within the line-length bound.
fn previous_line_start(source: &str, line_start: usize) -> Option<usize> {
    let end = line_start.checked_sub(1)?;
    // One extra byte keeps the separator before a line of exactly the bound.
    let window = end.saturating_sub(MAX_HERITAGE_LINE_BYTES.saturating_add(1));
    let bytes = source.as_bytes().get(window..end)?;
    match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(newline) => Some(window + newline + 1),
        None => (window == 0).then_some(0),
    }
}

/// Whether the current owner's declaration starts on zero-based `row` or later.
fn owner_starts_at_or_after(builder: &ExtractionBuilder<'_, '_>, row: usize) -> bool {
    let Some(owner) = builder.owners.last() else {
        return false;
    };
    builder
        .facts
        .symbols
        .iter()
        .rev()
        .find(|symbol| &symbol.id == owner)
        .is_some_and(|symbol| {
            usize::try_from(symbol.span.start_line()).is_ok_and(|line| line > row)
        })
}

/// The line starting at `line_start`, or `None` when it is longer than the bound.
fn physical_line(source: &str, line_start: usize) -> Option<&str> {
    let rest = source.get(line_start..)?;
    let length = rest
        .bytes()
        .take(MAX_HERITAGE_LINE_BYTES.saturating_add(1))
        .position(|byte| byte == b'\n')
        .or_else(|| (rest.len() <= MAX_HERITAGE_LINE_BYTES).then_some(rest.len()))?;
    rest.get(..length)
}

/// Emit the clause's targets, following ` _` line continuations (bounded). A
/// line whose clause is not exactly a list of dotted names ends the clause.
pub(super) fn emit_heritage_line(
    builder: &mut ExtractionBuilder<'_, '_>,
    line: &HeritageLine<'_>,
) -> Result<(), ExtractError> {
    if !line.owned {
        return Ok(());
    }
    let source = builder.context.snapshot.source();
    // The whole logical clause is validated before anything is emitted, so a
    // malformed continuation never leaves a partial heritage list behind.
    let mut lines = Vec::new();
    let mut current = Some(*line);
    while let Some(line) = current.take() {
        if lines.len() >= MAX_CONTINUATION_LINES {
            return Ok(());
        }
        let (body, continued) = split_continuation(line.clause);
        let targets = clause_targets(body);
        if targets.is_empty() {
            return Ok(());
        }
        if continued {
            let Some(next) = continuation_line(source, &line) else {
                return Ok(());
            };
            current = Some(next);
        }
        lines.push((line, targets));
    }
    for (line, targets) in &lines {
        emit_targets(builder, line, targets)?;
    }
    Ok(())
}

/// `(body, continued)`: a clause ending in a ` _` continuation keeps its body
/// without the marker and any trailing comma.
fn split_continuation(clause: &str) -> (&str, bool) {
    let trimmed = clause.trim_end();
    match trimmed.strip_suffix('_') {
        Some(body) if body.ends_with(char::is_whitespace) => {
            (body.trim_end().trim_end_matches(',').trim_end(), true)
        }
        _ => (clause, false),
    }
}

/// The physical line after `line`, read whole as a continued clause.
fn continuation_line<'source>(
    source: &'source str,
    line: &HeritageLine<'source>,
) -> Option<HeritageLine<'source>> {
    let current = physical_line(source, line.line_start)?;
    let line_start = line.line_start.checked_add(current.len())?.checked_add(1)?;
    let next = physical_line(source, line_start)?;
    Some(HeritageLine {
        kind: line.kind,
        clause: statement_code(next),
        clause_start: line_start,
        line_start,
        row: line.row.checked_add(1)?,
        owned: line.owned,
    })
}

/// Emit one heritage reference per target, skipping a target already recorded
/// for the same span (several recovered nodes can share one line).
fn emit_targets(
    builder: &mut ExtractionBuilder<'_, '_>,
    line: &HeritageLine<'_>,
    targets: &[(usize, &str)],
) -> Result<(), ExtractError> {
    for (offset, target) in targets.iter().take(MAX_HERITAGE_TARGETS) {
        builder.context.ensure_active()?;
        let start = line.clause_start + offset;
        let span = line_span(line, start, start + target.len())?;
        let owner = builder.owners.last().cloned();
        if !HeritageSeen::record(
            builder,
            HeritageKey {
                span,
                kind: line.kind,
                owner: owner.clone(),
            },
        )? {
            continue;
        }
        let name = builder.context.copy_text(target)?;
        builder.emit_reference(ExtractedReference {
            owner,
            name,
            resolution_name: None,
            kind: line.kind,
            span,
        })?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static HERITAGE_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `(offset, dotted name)` of each comma-separated clause target; the whole
/// clause abstains when any target is not a dotted identifier.
fn clause_targets(clause: &str) -> Vec<(usize, &str)> {
    let mut targets = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, byte) in clause.bytes().enumerate().chain([(clause.len(), b',')]) {
        match byte {
            b'(' => depth = depth.saturating_add(1),
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                let Some(target) = clause_target(clause, start, index) else {
                    return Vec::new();
                };
                targets.push(target);
                start = index + 1;
            }
            _ => {}
        }
    }
    // An unclosed `(Of ...)` leaves the final target unread: abstain entirely.
    if depth == 0 { targets } else { Vec::new() }
}

fn clause_target(clause: &str, start: usize, end: usize) -> Option<(usize, &str)> {
    let piece = clause.get(start..end)?;
    let leading = piece.len() - piece.trim_start().len();
    let trimmed = piece.trim();
    let head = trimmed.split('(').next().unwrap_or_default().trim_end();
    if !is_generic_suffix(&trimmed[head.len()..]) {
        return None;
    }
    let valid = !head.is_empty()
        && head.len() <= MAX_TARGET_BYTES
        && head.split('.').all(|segment| {
            segment
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && segment.bytes().any(|byte| byte.is_ascii_alphanumeric())
        });
    valid.then_some((start + leading, head))
}

/// Whether the text after a target's dotted head is empty or exactly one
/// balanced `(Of ...)` generic argument list (`Base(Of T) Extra` is not).
pub(super) fn is_generic_suffix(suffix: &str) -> bool {
    let suffix = suffix.trim();
    if suffix.is_empty() {
        return true;
    }
    let Some(arguments) = suffix.strip_prefix('(') else {
        return false;
    };
    let starts_with_of = arguments
        .trim_start()
        .get(..GENERIC_KEYWORD.len())
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case(GENERIC_KEYWORD));
    let mut depth = 1_usize;
    for (index, byte) in arguments.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return starts_with_of && index + 1 == arguments.len();
                }
            }
            _ => {}
        }
    }
    false
}

/// `Of ` opening a VB generic argument list.
const GENERIC_KEYWORD: &str = "of ";

fn line_span(
    line: &HeritageLine<'_>,
    start: usize,
    end: usize,
) -> Result<SourceSpan, ExtractError> {
    let row = u32::try_from(line.row)
        .ok()
        .and_then(|row| row.checked_add(1))
        .ok_or(ExtractError::InvalidSpan)?;
    let position = |byte: usize| {
        let column =
            u32::try_from(byte - line.line_start).map_err(|_| ExtractError::InvalidSpan)?;
        let byte = u64::try_from(byte).map_err(|_| ExtractError::InvalidSpan)?;
        SourcePosition::new(byte, row, column).map_err(|_| ExtractError::InvalidSpan)
    };
    SourceSpan::new(position(start)?, position(end)?).map_err(|_| ExtractError::InvalidSpan)
}

#[cfg(test)]
mod tests {
    use super::HERITAGE_WORK;
    use crate::{NativeExtractor, SourceLimits, SourceSnapshot};
    use cartograph_domain::ReferenceKind;
    use std::fmt::Write;

    fn heritage_work(width: usize) -> usize {
        let mut source = String::new();
        for index in 0..width {
            write!(
                source,
                "Class Child{index}\n Inherits Base_{index}\nEnd Class\n"
            )
            .unwrap_or_else(|error| panic!("test source failed: {error}"));
        }
        let snapshot = SourceSnapshot::from_bytes(
            "Wide.vb",
            source.as_bytes(),
            SourceLimits::new(1_048_576)
                .unwrap_or_else(|error| panic!("test setup failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("test setup failed: {error}"));
        HERITAGE_WORK.set(0);
        let extracted = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("test setup failed: {error}"))
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("test setup failed: {error}"));
        assert_eq!(
            extracted
                .references
                .iter()
                .filter(|reference| reference.kind == ReferenceKind::Extends)
                .count(),
            width
        );
        HERITAGE_WORK.get()
    }

    #[test]
    fn vbnet_wide_heritage_measures_bounded_deduplication_work() {
        for width in [192_usize, 384] {
            let work = heritage_work(width);
            // Recovery can revisit a site; every set lookup compares at most
            // a logarithmic number of keys, including insertion lookups.
            let bound = width
                * (usize::try_from(width.ilog2())
                    .unwrap_or_else(|error| panic!("test setup failed: {error}"))
                    + 1)
                * 6;
            assert!(
                work < bound,
                "{width} heritage sites used {work} comparisons (bound {bound})"
            );
        }
    }
}
