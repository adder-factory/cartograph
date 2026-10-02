//! Quote-aware field-qualified symbol-query parsing.

use cartograph_domain::{SourceLanguage, SymbolKind};

/// Numeric comparison accepted by the `centrality:` query qualifier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CentralityComparator {
    /// Require a score strictly above the threshold.
    GreaterThan,
    /// Require a score at or above the threshold.
    GreaterThanOrEqual,
    /// Require a score strictly below the threshold.
    LessThan,
    /// Require a score at or below the threshold.
    LessThanOrEqual,
}

/// One parsed `centrality:` predicate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CentralityFilter {
    comparator: CentralityComparator,
    value: f64,
}

impl CentralityFilter {
    /// Requested comparison operator.
    #[must_use]
    pub const fn comparator(self) -> CentralityComparator {
        self.comparator
    }

    /// Requested finite non-negative threshold.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.value
    }
}

/// Stable ordering requested by a field-qualified symbol query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualifiedSort {
    /// Preserve lexical relevance, then deterministic declaration order.
    Relevance,
    /// Rank directed `PageRank` descending, with unranked symbols last.
    Centrality,
}

/// Structured filters plus the remaining `ParadeDB` free-text query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedQualifiedQuery {
    text: String,
    kinds: Vec<String>,
    languages: Vec<String>,
    path_filters: Vec<String>,
    name_filters: Vec<String>,
    signature_filters: Vec<String>,
    callers_of: Vec<String>,
    callees_of: Vec<String>,
    depends_on: Vec<String>,
    centrality: Option<CentralityFilter>,
    sort: Option<QualifiedSort>,
}

impl ParsedQualifiedQuery {
    /// Free-text fragments which were not recognized as valid qualifiers.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// `kind:` values, OR-composed within the category.
    #[must_use]
    pub fn kinds(&self) -> &[String] {
        &self.kinds
    }

    /// `lang:` and `language:` values, OR-composed within the category.
    #[must_use]
    pub fn languages(&self) -> &[String] {
        &self.languages
    }

    /// Case-insensitive path substrings from `path:`.
    #[must_use]
    pub fn path_filters(&self) -> &[String] {
        &self.path_filters
    }

    /// Case-insensitive simple-name substrings from `name:`.
    #[must_use]
    pub fn name_filters(&self) -> &[String] {
        &self.name_filters
    }

    /// Case-insensitive signature substrings from `sig:` or `signature:`.
    #[must_use]
    pub fn signature_filters(&self) -> &[String] {
        &self.signature_filters
    }

    /// Target names used by `callers-of:`.
    #[must_use]
    pub fn callers_of(&self) -> &[String] {
        &self.callers_of
    }

    /// Source names used by `callees-of:`.
    #[must_use]
    pub fn callees_of(&self) -> &[String] {
        &self.callees_of
    }

    /// Target names used by `depends-on:`.
    #[must_use]
    pub fn depends_on(&self) -> &[String] {
        &self.depends_on
    }

    /// Latest valid centrality predicate, when present.
    #[must_use]
    pub const fn centrality(&self) -> Option<CentralityFilter> {
        self.centrality
    }

    /// Explicit result ordering, when present.
    #[must_use]
    pub const fn sort(&self) -> Option<QualifiedSort> {
        self.sort
    }

    /// Whether at least one recognized hard filter is present.
    #[must_use]
    pub fn has_filters(&self) -> bool {
        !self.kinds.is_empty()
            || !self.languages.is_empty()
            || !self.path_filters.is_empty()
            || !self.name_filters.is_empty()
            || !self.signature_filters.is_empty()
            || !self.callers_of.is_empty()
            || !self.callees_of.is_empty()
            || !self.depends_on.is_empty()
            || self.centrality.is_some()
    }
}

/// Parse v1-compatible field qualifiers while preserving invalid/unknown tokens as search text.
///
/// Tokens are whitespace-delimited except inside double quotes. An unterminated quote consumes
/// the remaining input, matching v1's forgiving behavior. Recognized categories compose as an
/// intersection, while repeated values inside one category are OR alternatives.
#[must_use]
pub fn parse_qualified_query(raw: &str) -> ParsedQualifiedQuery {
    let mut parsed = ParsedQualifiedQuery::default();
    let mut text = Vec::new();
    for token in quote_aware_tokens(raw) {
        if !classify_token(token, &mut parsed) {
            text.push(token);
        }
    }
    let mut normalized_text = text.join(" ");
    let leading_whitespace = normalized_text.len() - normalized_text.trim_start().len();
    let trimmed_length = normalized_text.trim().len();
    normalized_text.truncate(leading_whitespace + trimmed_length);
    normalized_text.drain(..leading_whitespace);
    parsed.text = normalized_text;
    parsed
}

fn classify_token(token: &str, parsed: &mut ParsedQualifiedQuery) -> bool {
    let Some((key, value)) = qualified_token_parts(token) else {
        return false;
    };
    match key.as_str() {
        "kind" => SymbolKind::from_stable_str(value).is_some() && push(&mut parsed.kinds, value),
        "lang" | "language" => classify_language(value, parsed),
        "path" => push(&mut parsed.path_filters, value),
        "name" => push(&mut parsed.name_filters, value),
        "sig" | "signature" => push(&mut parsed.signature_filters, value),
        "callers-of" => push(&mut parsed.callers_of, value),
        "callees-of" => push(&mut parsed.callees_of, value),
        "depends-on" => push(&mut parsed.depends_on, value),
        "centrality" => classify_centrality(value, parsed),
        "sort" => classify_sort(value, parsed),
        _ => false,
    }
}

fn qualified_token_parts(token: &str) -> Option<(String, &str)> {
    let (key, raw_value) = token.split_once(':')?;
    if key.is_empty() || raw_value.is_empty() {
        return None;
    }
    let value = unquote(raw_value);
    (!value.is_empty()).then(|| (key.to_ascii_lowercase(), value))
}

fn classify_language(value: &str, parsed: &mut ParsedQualifiedQuery) -> bool {
    let lowered = value.to_ascii_lowercase();
    SourceLanguage::from_stable_str(&lowered).is_some()
        && push_owned(&mut parsed.languages, lowered)
}

fn classify_centrality(value: &str, parsed: &mut ParsedQualifiedQuery) -> bool {
    let Some(filter) = parse_centrality(value) else {
        return false;
    };
    parsed.centrality = Some(filter);
    true
}

fn classify_sort(value: &str, parsed: &mut ParsedQualifiedQuery) -> bool {
    parsed.sort = if value.eq_ignore_ascii_case("centrality") {
        Some(QualifiedSort::Centrality)
    } else if value.eq_ignore_ascii_case("relevance") {
        Some(QualifiedSort::Relevance)
    } else {
        return false;
    };
    true
}

fn push(values: &mut Vec<String>, value: &str) -> bool {
    values.push(value.to_owned());
    true
}

fn push_owned(values: &mut Vec<String>, value: String) -> bool {
    values.push(value);
    true
}

fn unquote(value: &str) -> &str {
    value.strip_circumfix('"', '"').unwrap_or(value)
}

fn parse_centrality(value: &str) -> Option<CentralityFilter> {
    let (comparator, numeric) = if let Some(value) = value.strip_prefix(">=") {
        (CentralityComparator::GreaterThanOrEqual, value)
    } else if let Some(value) = value.strip_prefix("<=") {
        (CentralityComparator::LessThanOrEqual, value)
    } else if let Some(value) = value.strip_prefix('>') {
        (CentralityComparator::GreaterThan, value)
    } else if let Some(value) = value.strip_prefix('<') {
        (CentralityComparator::LessThan, value)
    } else {
        (CentralityComparator::GreaterThanOrEqual, value)
    };
    valid_decimal(numeric)
        .then(|| numeric.parse::<f64>().ok())
        .flatten()
        .and_then(|value| {
            value
                .is_finite()
                .then_some(CentralityFilter { comparator, value })
        })
}

fn valid_decimal(value: &str) -> bool {
    let mut pieces = value.split('.');
    let Some(integer) = pieces.next() else {
        return false;
    };
    if integer.is_empty() || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (pieces.next(), pieces.next()) {
        (None, None) => true,
        (Some(fraction), None) => {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

fn quote_aware_tokens(raw: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (offset, character) in raw.char_indices() {
        if start.is_none() {
            if character.is_whitespace() {
                continue;
            }
            start = Some(offset);
        }
        if character == '"' {
            quoted = !quoted;
        } else if character.is_whitespace()
            && !quoted
            && let Some(token_start) = start.take()
        {
            tokens.push(&raw[token_start..offset]);
        }
    }
    if let Some(token_start) = start {
        tokens.push(&raw[token_start..]);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_v1_qualifier_and_preserves_unknown_text() {
        let parsed = parse_qualified_query(
            "kind:function kind:method lang:TYPESCRIPT path:\"src/api routes\" \
             name:Handler sig:Promise<User> callers-of:authenticate callees-of:bootstrap \
             depends-on:Base centrality:>0.01 sort:CENTRALITY TODO: review",
        );
        assert_eq!(parsed.kinds(), ["function", "method"]);
        assert_eq!(parsed.languages(), ["typescript"]);
        assert_eq!(parsed.path_filters(), ["src/api routes"]);
        assert_eq!(parsed.name_filters(), ["Handler"]);
        assert_eq!(parsed.signature_filters(), ["Promise<User>"]);
        assert_eq!(parsed.callers_of(), ["authenticate"]);
        assert_eq!(parsed.callees_of(), ["bootstrap"]);
        assert_eq!(parsed.depends_on(), ["Base"]);
        assert_eq!(
            parsed.centrality(),
            Some(CentralityFilter {
                comparator: CentralityComparator::GreaterThan,
                value: 0.01,
            })
        );
        assert_eq!(parsed.sort(), Some(QualifiedSort::Centrality));
        assert_eq!(parsed.text(), "TODO: review");
    }

    #[test]
    fn malformed_known_values_fall_through_and_latest_centrality_wins() {
        let parsed = parse_qualified_query(
            "kind:not-real lang:not-real centrality:.5 centrality:>0.1 centrality:<=0.9 \
             sort:recent kind:union kind:table",
        );
        assert_eq!(parsed.kinds(), ["union", "table"]);
        assert_eq!(
            parsed.text(),
            "kind:not-real lang:not-real centrality:.5 sort:recent"
        );
        assert_eq!(
            parsed.centrality(),
            Some(CentralityFilter {
                comparator: CentralityComparator::LessThanOrEqual,
                value: 0.9,
            })
        );
    }

    #[test]
    fn empty_filter_only_plain_and_unterminated_quote_queries_are_forgiving() {
        let empty = parse_qualified_query("");
        assert_eq!(empty.text(), "");
        assert!(!empty.has_filters());

        let filters = parse_qualified_query("kind:function language:rust");
        assert_eq!(filters.text(), "");
        assert!(filters.has_filters());

        let unterminated = parse_qualified_query("path:\"src/my dir remaining words");
        assert_eq!(
            unterminated.path_filters(),
            ["\"src/my dir remaining words"]
        );
    }
}
