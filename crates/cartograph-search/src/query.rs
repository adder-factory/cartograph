use std::collections::BTreeSet;

use crate::CONTEXT_QUERY_MAXIMUM_BYTES;

const MAXIMUM_ALIASES: usize = 8;
const QUESTION_WORDS: &[&str] = &[
    "a", "an", "are", "does", "for", "how", "i", "in", "is", "it", "of", "please", "some", "that",
    "the", "this", "to", "we", "what", "which", "with",
];

/// Build only the lexical view. The caller retains the original question for models and intent.
pub(crate) fn expand_code_query(query: &str) -> String {
    let tokens = query.split_whitespace().collect::<Vec<_>>();
    let compact = tokens.len() >= 6
        && tokens
            .iter()
            .enumerate()
            .filter(|(index, _)| is_question_word(&tokens, *index))
            .count()
            >= 2
        && !query.contains(['"', '`']);
    let mut expanded = if compact {
        tokens
            .iter()
            .enumerate()
            .filter(|(index, _)| !is_question_word(&tokens, *index))
            .map(|(_, token)| *token)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        query.to_owned()
    };
    let normalized = query
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let mut seen = normalized
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut added = 0;
    for term in normalized.split_ascii_whitespace() {
        for alias in code_query_aliases(term) {
            if added == MAXIMUM_ALIASES
                || seen.contains(*alias)
                || expanded.len().saturating_add(alias.len()).saturating_add(1)
                    > CONTEXT_QUERY_MAXIMUM_BYTES
            {
                continue;
            }
            expanded.push(' ');
            expanded.push_str(alias);
            seen.insert((*alias).to_owned());
            added += 1;
        }
    }
    expanded
}

fn is_question_word(tokens: &[&str], index: usize) -> bool {
    let token = tokens[index].trim_matches(['?', '.', ',', ';', '!']);
    // Capitalization and an explicit code-name position take precedence over filler removal.
    let question_word = QUESTION_WORDS.contains(&token)
        || (index == 0 && ["How", "What", "Which", "Please", "Does", "Are", "Is"].contains(&token));
    question_word
        && index.checked_sub(1).is_none_or(|previous| {
            ![
                "function",
                "method",
                "symbol",
                "type",
                "parameter",
                "field",
                "variable",
                "module",
                "class",
                "struct",
                "trait",
                "enum",
                "variant",
                "constant",
                "macro",
                "identifier",
                "argument",
                "property",
                "interface",
                "namespace",
                "package",
                "named",
                "called",
            ]
            .iter()
            .any(|introducer| tokens[previous].eq_ignore_ascii_case(introducer))
        })
}

fn code_query_aliases(term: &str) -> &'static [&'static str] {
    match term {
        "bm25" => &["lexical", "postings"],
        "check" | "checked" | "checking" | "checks" => &["validate", "verify"],
        "run" | "running" | "runs" => &["execute"],
        "fresh" | "freshness" => &["source_revision", "digest_version", "scan_source"],
        "checkout" | "checkouts" => &["source", "project_root"],
        "match" | "matched" | "matches" | "matching" => &["compare", "digest"],
        "immutable" => &["content_digest", "generation"],
        "embedding" | "embeddings" | "vector" | "vectors" => &["embed", "embedding"],
        "missing" => &["pending"],
        "owner" | "ownership" => &["ipc", "proxy"],
        "forward" | "forwarding" => &["proxy"],
        "debounce" | "debouncing" => &["watcher", "debounce_events"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::expand_code_query;

    #[test]
    fn lexical_questions_drop_filler_but_keep_identifiers_and_constraints() {
        let view = expand_code_query(
            "How does OwnerClient::connect refuse clients without a same-user identity before opening?",
        );
        assert!(expand_code_query("how does the _is_ identifier work").contains("_is_"));
        assert!(view.starts_with(
            "OwnerClient::connect refuse clients without same-user identity before opening?"
        ));
        for constraint in ["not", "never", "without", "before", "after", "only"] {
            assert!(
                expand_code_query(&format!("how does the reader {constraint} publish"))
                    .split_whitespace()
                    .any(|term| term == constraint)
            );
        }
    }

    #[test]
    fn short_and_quoted_searches_keep_their_original_lexical_prefix() {
        for query in [
            "OwnerClient::connect",
            "src/cache/tests.rs",
            "\"the current generation\"",
            "find `the` in a function",
        ] {
            assert!(expand_code_query(query).starts_with(query), "{query}");
        }
    }

    #[test]
    fn lexical_questions_preserve_capitalized_and_explicit_code_names() {
        for (query, name) in [
            (
                "How does Some return values without allocation in Rust?",
                "Some",
            ),
            ("How does the function a reject invalid input?", "a"),
            ("How does the type Is represent a predicate?", "Is"),
            ("How does the parameter named is affect validation?", "is"),
            ("How does the symbol THE store its contents?", "THE"),
        ] {
            let expanded = expand_code_query(query);
            assert!(
                expanded.split_whitespace().any(|token| token == name),
                "{query}: {expanded}"
            );
        }
    }

    #[test]
    fn concept_expansions_are_deduplicated_and_bounded_without_sql_aliases() {
        let view = expand_code_query(
            "BM25 bm25 owner ownership vectors embeddings debounce missing fresh check match immutable",
        );
        for term in ["lexical", "postings", "ipc", "proxy", "embed"] {
            assert_eq!(
                view.split_whitespace().filter(|word| *word == term).count(),
                1
            );
        }
        assert_eq!(view.split_whitespace().count(), 12 + super::MAXIMUM_ALIASES);
        assert!(!view.contains("relation"));
        assert!(!view.contains("catalog"));
    }
}
