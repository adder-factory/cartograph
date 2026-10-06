//! Bounded Rust/Go directory conventions; proximity does not break ambiguity.

use super::{
    FrameworkCandidatePattern, FrameworkConventionInput, FrameworkNamePattern, FrameworkRule,
    ResolutionCandidate, ResolutionFileContext, SymbolKind, Visibility, framework_rule_score,
};

const CONVENTION_SCORE: u8 = 90;

const RUST_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::PrefixesOrSuffixes {
            prefixes: &["handle_"],
            suffixes: &["_handler"],
            minimum_length: 8,
        },
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[SymbolKind::Function]),
        directories: &["handlers", "handler", "api", "routes", "controllers"],
        score: CONVENTION_SCORE,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Service", "Repository"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[
            SymbolKind::Struct,
            SymbolKind::Trait,
        ]),
        directories: &["services", "service", "repository", "domain"],
        score: CONVENTION_SCORE,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[SymbolKind::Struct]),
        directories: &["models", "model", "entities", "entity", "domain", "types"],
        score: CONVENTION_SCORE,
    },
];

const GO_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::PrefixesOrSuffixes {
            prefixes: &["Handle"],
            suffixes: &["Handler"],
            minimum_length: 7,
        },
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[SymbolKind::Function]),
        directories: &[
            "handler",
            "handlers",
            "api",
            "routes",
            "controller",
            "controllers",
        ],
        score: CONVENTION_SCORE,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Service", "Repository", "Store"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[
            SymbolKind::Struct,
            SymbolKind::Interface,
        ]),
        directories: &["service", "services", "repository", "store", "pkg"],
        score: CONVENTION_SCORE,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PrefixesOrSuffixes {
            prefixes: &["Auth", "Log"],
            suffixes: &["Middleware"],
            minimum_length: 4,
        },
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[SymbolKind::Function]),
        directories: &["middleware", "middlewares"],
        score: CONVENTION_SCORE,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[SymbolKind::Struct]),
        directories: &["model", "models", "entity", "entities", "domain", "pkg"],
        score: CONVENTION_SCORE,
    },
];

fn rules(language: &str) -> &'static [FrameworkRule] {
    match language {
        "rust" => RUST_RULES,
        "go" => GO_RULES,
        _ => &[],
    }
}

pub(super) fn reference(source: &ResolutionFileContext, name: &str) -> bool {
    rules(&source.language)
        .iter()
        .any(|rule| rule.name.matches(name))
}

pub(super) fn score(input: &FrameworkConventionInput<'_>) -> u8 {
    if input.source.language == "rust"
        && (!same_crate(&input.source.path, &input.target.path) || !input.candidate.export.exported)
    {
        return 0;
    }
    framework_rule_score(input, rules(&input.source.language))
}

/// An unexported Go declaration remains accessible from its own package.
pub(super) fn visible(
    source: &ResolutionFileContext,
    target: &ResolutionFileContext,
    candidate: &ResolutionCandidate,
) -> bool {
    candidate.visibility != Some(Visibility::Private)
        || (source.language == "go"
            && target.language == "go"
            && source.directory == target.directory
            && source.package.is_some()
            && source.package == target.package)
}

fn same_crate(source: &str, target: &str) -> bool {
    let directory = |path: &str| {
        path.rsplit_once("/src/")
            .map(|(root, _)| root.len())
            .or_else(|| path.starts_with("src/").then_some(0))
    };
    match (directory(source), directory(target)) {
        (Some(left), Some(right)) => source[..left] == target[..right],
        _ => false,
    }
}
