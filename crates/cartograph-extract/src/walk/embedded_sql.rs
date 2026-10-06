use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SourceLanguage};
use tree_sitter::Node;

use crate::{
    EMBEDDED_SQL_RESOLUTION_PREFIX, ExtractError, ExtractedReference,
    source_lines::{LineMap, SourceByteRange},
    walk::{
        AstVisitBudget, ExtractionBuilder, owner_for_node,
        syntax::{descendants_including_root, named_children},
    },
};

const MAX_AST_DEPTH: usize = 256;
const MAX_LITERAL_BYTES: usize = 1024 * 1024;
const MAX_TOKENS_PER_LITERAL: usize = 65_536;
const MAX_REFERENCES_PER_FILE: usize = 4_096;
const MAX_IDENTIFIER_PARTS: usize = 3;
const INITIAL_SQL_HIT_CAPACITY: usize = 8;
const TRIPLE_QUOTE_WIDTH: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SqlOperation {
    Read,
    Write,
    Ddl,
}

impl SqlOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Ddl => "ddl",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Word,
    QuotedIdentifier,
    Dot,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: TokenKind,
    source_start: usize,
    source_end: usize,
    value_start: usize,
    value_end: usize,
}

impl Token {
    fn value(self, source: &str) -> &str {
        source
            .get(self.value_start..self.value_end)
            .unwrap_or_default()
    }

    fn is_keyword(self, source: &str, keyword: &str) -> bool {
        self.kind == TokenKind::Word && self.value(source).eq_ignore_ascii_case(keyword)
    }

    const fn identifier(self) -> bool {
        matches!(self.kind, TokenKind::Word | TokenKind::QuotedIdentifier)
    }
}

struct SqlHit {
    name: String,
    operation: SqlOperation,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct SqlTokenSite<'a> {
    tokens: &'a [Token],
    source: &'a str,
    index: usize,
    token: Token,
}

impl SqlTokenSite<'_> {
    fn keyword_at(self, index: usize, keyword: &str) -> bool {
        SqlTokenStream {
            tokens: self.tokens,
            source: self.source,
        }
        .keyword_at(index, keyword)
    }
}

struct LiteralScanInput<'tree, 'scan> {
    node: Node<'tree>,
    depth: usize,
    lines: &'scan LineMap,
    budget: &'scan mut ScanBudget,
}

struct LiteralInput<'tree, 'scan> {
    node: Node<'tree>,
    lines: &'scan LineMap,
    budget: &'scan mut ScanBudget,
}

#[derive(Clone, Copy)]
struct QuotedInput<'source> {
    bytes: &'source [u8],
    start: usize,
    close: u8,
}

#[derive(Clone, Copy)]
struct SqlTokenStream<'source> {
    tokens: &'source [Token],
    source: &'source str,
}

impl SqlTokenStream<'_> {
    fn keyword_at(self, index: usize, keyword: &str) -> bool {
        self.tokens
            .get(index)
            .is_some_and(|token| token.is_keyword(self.source, keyword))
    }
}

#[derive(Default)]
struct ScanBudget {
    visits: AstVisitBudget<MAX_AST_DEPTH>,
    references: usize,
}

impl ScanBudget {
    fn admit_reference(&mut self) -> Result<(), ExtractError> {
        self.references = self
            .references
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.references > MAX_REFERENCES_PER_FILE {
            Err(ExtractError::OutputLimit)
        } else {
            Ok(())
        }
    }
}

pub(super) fn enrich(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if !supported_language(builder.context.snapshot.language()) {
        return Ok(());
    }
    let lines = LineMap::new(builder.context.source())?;
    let mut budget = ScanBudget::default();
    scan_literals(
        builder,
        LiteralScanInput {
            node: root,
            depth: 0,
            lines: &lines,
            budget: &mut budget,
        },
    )
}

fn supported_language(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::TypeScript
            | SourceLanguage::JavaScript
            | SourceLanguage::Tsx
            | SourceLanguage::Jsx
            | SourceLanguage::Python
            | SourceLanguage::Go
            | SourceLanguage::Rust
            | SourceLanguage::Java
            | SourceLanguage::Kotlin
            | SourceLanguage::CSharp
            | SourceLanguage::Php
            | SourceLanguage::Ruby
            | SourceLanguage::Sql
    )
}

fn scan_literals(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LiteralScanInput<'_, '_>,
) -> Result<(), ExtractError> {
    let LiteralScanInput {
        node,
        depth,
        lines,
        budget,
    } = input;
    budget.visits.observe(builder, depth)?;
    let language = builder.context.snapshot.language();
    if string_literal_node(node, language)
        && !node
            .parent()
            .is_some_and(|parent| string_literal_node(parent, language))
        && !contains_dynamic_fragment(node)
    {
        scan_literal(
            builder,
            LiteralInput {
                node,
                lines,
                budget,
            },
        )?;
        return Ok(());
    }
    for child in named_children(node) {
        scan_literals(
            builder,
            LiteralScanInput {
                node: child,
                depth: depth.saturating_add(1),
                lines,
                budget: &mut *budget,
            },
        )?;
    }
    Ok(())
}

fn string_literal_node(node: Node<'_>, language: SourceLanguage) -> bool {
    let kind = node.kind();
    matches!(
        kind,
        "string"
            | "string_literal"
            | "raw_string_literal"
            | "interpreted_string_literal"
            | "template_string"
            | "line_string_literal"
            | "multi_line_string_literal"
            | "heredoc_body"
            | "nowdoc_body"
    ) || (language == SourceLanguage::Sql && kind == "literal")
        || (kind.contains("string")
            && !kind.contains("interpolation")
            && !kind.contains("fragment")
            && !kind.contains("escape"))
}

fn contains_dynamic_fragment(node: Node<'_>) -> bool {
    descendants_including_root(node).any(|current| {
        let kind = current.kind();
        kind.contains("interpolation")
            || kind.contains("substitution")
            || kind.contains("format_expression")
            || kind.contains("embedded_expression")
    })
}

fn scan_literal(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LiteralInput<'_, '_>,
) -> Result<(), ExtractError> {
    let LiteralInput {
        node,
        lines,
        budget,
    } = input;
    let raw = builder.context.text(node);
    let Some((content_start, content_end)) = literal_content_bounds(raw) else {
        return Ok(());
    };
    let Some(content) = raw.get(content_start..content_end) else {
        return Ok(());
    };
    if content.len() > MAX_LITERAL_BYTES {
        return Err(ExtractError::OutputLimit);
    }
    if content.contains("${") || super::specifier_safety::specifier_may_carry_credential(content) {
        return Ok(());
    }
    let tokens = tokenize(content)?;
    if tokens.is_empty() || !has_sql_anchor(&tokens, content) {
        return Ok(());
    }
    let hits = collect_hits(&tokens, content)?;
    let owner = owner_for_node(builder, node);
    let source_len = builder.context.source().len();
    let base = node
        .start_byte()
        .checked_add(content_start)
        .ok_or(ExtractError::InvalidSpan)?;
    for hit in hits {
        builder.context.ensure_active()?;
        budget.admit_reference()?;
        let start = base
            .checked_add(hit.start)
            .ok_or(ExtractError::InvalidSpan)?;
        let end = base.checked_add(hit.end).ok_or(ExtractError::InvalidSpan)?;
        let resolution_name = resolution_name(builder, hit.operation, &hit.name)?;
        builder.emit_reference(ExtractedReference {
            owner: owner.clone(),
            name: hit.name,
            resolution_name: Some(resolution_name),
            kind: ReferenceKind::References,
            span: lines.span(SourceByteRange::new(start, end, source_len))?,
        })?;
    }
    Ok(())
}

fn literal_content_bounds(raw: &str) -> Option<(usize, usize)> {
    if raw.is_empty() {
        return None;
    }
    let bytes = raw.as_bytes();
    let first = bytes
        .iter()
        .position(|byte| matches!(byte, b'\'' | b'"' | b'`'));
    let Some(first) = first else {
        return Some((0, raw.len()));
    };
    let delimiter = bytes[first];
    let triple = bytes
        .get(first..first.saturating_add(TRIPLE_QUOTE_WIDTH))
        .is_some_and(|value| value == [delimiter, delimiter, delimiter]);
    let width = if triple { TRIPLE_QUOTE_WIDTH } else { 1 };
    let start = first.checked_add(width)?;
    let end = if triple {
        bytes
            .array_windows()
            .rposition(|window| *window == [delimiter, delimiter, delimiter])?
    } else {
        bytes.iter().rposition(|byte| *byte == delimiter)?
    };
    (start <= end).then_some((start, end))
}

fn skipped_sql_region(bytes: &[u8], index: usize) -> Option<usize> {
    let byte = bytes[index];
    if byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'(' | b')') {
        Some(index.saturating_add(1))
    } else if byte == b'-' && bytes.get(index.saturating_add(1)) == Some(&b'-') {
        Some(skip_line_comment(bytes, index.saturating_add(2)))
    } else if byte == b'/' && bytes.get(index.saturating_add(1)) == Some(&b'*') {
        Some(skip_block_comment(bytes, index.saturating_add(2)))
    } else if byte == b'\'' {
        Some(quoted_end(QuotedInput {
            bytes,
            start: index,
            close: b'\'',
        }))
    } else {
        None
    }
}

fn sql_punctuation_token(byte: u8, index: usize) -> Option<Token> {
    (byte == b'.').then_some(Token {
        kind: TokenKind::Dot,
        source_start: index,
        source_end: index.saturating_add(1),
        value_start: index,
        value_end: index.saturating_add(1),
    })
}

fn quoted_sql_identifier(bytes: &[u8], index: usize) -> Option<(Token, usize)> {
    let byte = bytes[index];
    if !matches!(byte, b'"' | b'`' | b'[') {
        return None;
    }
    let close = if byte == b'[' { b']' } else { byte };
    let end = quoted_end(QuotedInput {
        bytes,
        start: index,
        close,
    });
    (end > index.saturating_add(1)).then_some((
        Token {
            kind: TokenKind::QuotedIdentifier,
            source_start: index,
            source_end: end,
            value_start: index.saturating_add(1),
            value_end: end.saturating_sub(1),
        },
        end,
    ))
}

fn tokenize(source: &str) -> Result<Vec<Token>, ExtractError> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    tokens
        .try_reserve(64)
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut index = 0_usize;
    while index < bytes.len() {
        if tokens.len() >= MAX_TOKENS_PER_LITERAL {
            return Err(ExtractError::OutputLimit);
        }
        let byte = bytes[index];
        if let Some(next) = skipped_sql_region(bytes, index) {
            index = next;
            continue;
        }
        if let Some(token) = sql_punctuation_token(byte, index) {
            tokens.push(token);
            index = index.saturating_add(1);
            continue;
        }
        if let Some((token, end)) = quoted_sql_identifier(bytes, index) {
            tokens.push(token);
            index = end;
            continue;
        }
        if identifier_start(byte) {
            let start = index;
            index = index.saturating_add(1);
            while bytes
                .get(index)
                .is_some_and(|byte| identifier_continue(*byte))
            {
                index = index.saturating_add(1);
            }
            tokens.push(Token {
                kind: TokenKind::Word,
                source_start: start,
                source_end: index,
                value_start: start,
                value_end: index,
            });
            continue;
        }
        index = index.saturating_add(1);
    }
    Ok(tokens)
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
        index = index.saturating_add(1);
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        if bytes.get(index) == Some(&b'*') && bytes.get(index.saturating_add(1)) == Some(&b'/') {
            return index.saturating_add(2);
        }
        index = index.saturating_add(1);
    }
    index
}

fn quoted_end(input: QuotedInput<'_>) -> usize {
    let mut index = input.start.saturating_add(1);
    while index < input.bytes.len() {
        if input.bytes[index] == input.close {
            if input.bytes.get(index.saturating_add(1)) == Some(&input.close) {
                index = index.saturating_add(2);
                continue;
            }
            return index.saturating_add(1);
        }
        index = index.saturating_add(1);
    }
    input.bytes.len()
}

fn identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn identifier_continue(byte: u8) -> bool {
    matches!(byte, b'_' | b'$' | b'-') || byte.is_ascii_alphanumeric()
}

fn has_sql_anchor(tokens: &[Token], source: &str) -> bool {
    tokens.iter().any(|token| {
        [
            "select", "insert", "update", "delete", "merge", "create", "alter", "drop", "truncate",
            "with",
        ]
        .iter()
        .any(|keyword| token.is_keyword(source, keyword))
    })
}

fn collect_hits(tokens: &[Token], source: &str) -> Result<Vec<SqlHit>, ExtractError> {
    let mut hits = empty_hit_buffer()?;
    let mut seen = BTreeSet::new();
    for (index, token) in tokens.iter().copied().enumerate() {
        let Some(hit) = sql_hit_at(SqlTokenSite {
            tokens,
            source,
            index,
            token,
        })?
        else {
            continue;
        };
        if seen.insert(sql_hit_identity(&hit)) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn empty_hit_buffer() -> Result<Vec<SqlHit>, ExtractError> {
    let mut hits = Vec::new();
    hits.try_reserve(INITIAL_SQL_HIT_CAPACITY)
        .map_err(|_| ExtractError::OutputLimit)?;
    Ok(hits)
}

fn sql_hit_identity(hit: &SqlHit) -> (String, SqlOperation) {
    (hit.name.to_ascii_lowercase(), hit.operation)
}

fn sql_hit_at(site: SqlTokenSite<'_>) -> Result<Option<SqlHit>, ExtractError> {
    let Some((operation, start)) = sql_target_at(site) else {
        return Ok(None);
    };
    let Some((name, start, end)) = parse_identifier(site.tokens, site.source, start)? else {
        return Ok(None);
    };
    if reserved_name(name.rsplit('.').next().unwrap_or(&name)) {
        return Ok(None);
    }
    Ok(Some(SqlHit {
        name,
        operation,
        start,
        end,
    }))
}

fn sql_target_at(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    read_target(site)
        .or_else(|| write_target(site))
        .or_else(|| schema_target(site))
}

fn read_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    (site.token.is_keyword(site.source, "from") || site.token.is_keyword(site.source, "join"))
        .then_some((SqlOperation::Read, site.index.saturating_add(1)))
}

fn write_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    if site.token.is_keyword(site.source, "update") {
        return Some((SqlOperation::Write, site.index.saturating_add(1)));
    }
    insert_target(site).or_else(|| separated_write_target(site))
}

fn insert_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    let following = site.index.saturating_add(1);
    (site.token.is_keyword(site.source, "insert") && site.keyword_at(following, "into"))
        .then_some((SqlOperation::Write, site.index.saturating_add(2)))
}

fn separated_write_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    write_target_has_separator(site).then_some((SqlOperation::Write, site.index.saturating_add(2)))
}

fn write_target_has_separator(site: SqlTokenSite<'_>) -> bool {
    let following = site.index.saturating_add(1);
    (site.token.is_keyword(site.source, "delete") && site.keyword_at(following, "from"))
        || (site.token.is_keyword(site.source, "merge") && site.keyword_at(following, "into"))
}

fn schema_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    create_schema_target(site).or_else(|| alter_schema_target(site))
}

fn create_schema_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    if site.token.is_keyword(site.source, "create") {
        create_target(site.tokens, site.source, site.index.saturating_add(1))
    } else {
        None
    }
}

fn alter_schema_target(site: SqlTokenSite<'_>) -> Option<(SqlOperation, usize)> {
    let following = site.index.saturating_add(1);
    let ddl = site.token.is_keyword(site.source, "alter")
        || site.token.is_keyword(site.source, "truncate")
        || site.token.is_keyword(site.source, "drop");
    if ddl {
        ddl_target(site.tokens, site.source, following)
    } else {
        None
    }
}

fn create_target(
    tokens: &[Token],
    source: &str,
    mut index: usize,
) -> Option<(SqlOperation, usize)> {
    let stream = SqlTokenStream { tokens, source };
    if stream.keyword_at(index, "or") && stream.keyword_at(index.saturating_add(1), "replace") {
        index = index.saturating_add(2);
    }
    if stream.keyword_at(index, "temp") || stream.keyword_at(index, "temporary") {
        index = index.saturating_add(1);
    }
    ddl_target(tokens, source, index)
}

fn ddl_target(tokens: &[Token], source: &str, index: usize) -> Option<(SqlOperation, usize)> {
    let stream = SqlTokenStream { tokens, source };
    if stream.keyword_at(index, "table") || stream.keyword_at(index, "view") {
        let mut target = index.saturating_add(1);
        if stream.keyword_at(target, "if") {
            target = target.saturating_add(1);
            if stream.keyword_at(target, "not") {
                target = target.saturating_add(1);
            }
            if stream.keyword_at(target, "exists") {
                target = target.saturating_add(1);
            }
        }
        Some((SqlOperation::Ddl, target))
    } else {
        None
    }
}

fn parse_identifier(
    tokens: &[Token],
    source: &str,
    start: usize,
) -> Result<Option<(String, usize, usize)>, ExtractError> {
    let Some(first) = tokens
        .get(start)
        .copied()
        .filter(|token| token.identifier())
    else {
        return Ok(None);
    };
    let mut parts = vec![first.value(source)];
    let mut end = first.source_end;
    let mut index = start.saturating_add(1);
    while parts.len() < MAX_IDENTIFIER_PARTS
        && tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Dot)
        && tokens
            .get(index.saturating_add(1))
            .is_some_and(|token| token.identifier())
    {
        let Some(part) = tokens.get(index.saturating_add(1)).copied() else {
            break;
        };
        parts.push(part.value(source));
        end = part.source_end;
        index = index.saturating_add(2);
    }
    let length = parts
        .iter()
        .try_fold(parts.len().saturating_sub(1), |length, part| {
            length.checked_add(part.len())
        });
    let mut name = String::new();
    name.try_reserve(length.ok_or(ExtractError::OutputLimit)?)
        .map_err(|_| ExtractError::OutputLimit)?;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            name.push('.');
        }
        name.push_str(part.trim_matches('\\'));
    }
    Ok(Some((name, first.source_start, end)))
}

fn reserved_name(name: &str) -> bool {
    [
        "a", "an", "the", "where", "on", "group", "order", "limit", "using", "as", "select",
        "into", "values", "set", "and", "or", "not", "null", "true", "false",
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn resolution_name(
    builder: &ExtractionBuilder<'_, '_>,
    operation: SqlOperation,
    table: &str,
) -> Result<String, ExtractError> {
    let length = EMBEDDED_SQL_RESOLUTION_PREFIX
        .len()
        .checked_add(operation.as_str().len())
        .and_then(|length| length.checked_add(2))
        .and_then(|length| length.checked_add(table.len()))
        .ok_or(ExtractError::OutputLimit)?;
    let mut name = String::new();
    name.try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    name.push_str(EMBEDDED_SQL_RESOLUTION_PREFIX);
    name.push_str(operation.as_str());
    name.push_str("::");
    name.push_str(table);
    builder.context.copy_text(&name)
}
