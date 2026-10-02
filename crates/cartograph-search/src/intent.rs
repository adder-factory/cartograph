use serde::Serialize;

use crate::RetrievalPreference;

const ERROR_TERMS: &[&str] = &[
    "bug",
    "crash",
    "debug",
    "diagnose",
    "error",
    "exception",
    "fail",
    "failing",
    "failure",
    "panic",
    "stuck",
];
const TEST_TERMS: &[&str] = &["test", "tests", "coverage", "verify", "verification"];
const DOCUMENTATION_TERMS: &[&str] = &[
    "doc",
    "docs",
    "documentation",
    "example",
    "examples",
    "guide",
    "readme",
];
const ARCHITECTURE_TERMS: &[&str] = &[
    "architecture",
    "boundary",
    "boundaries",
    "dependency",
    "dependencies",
    "design",
    "overview",
    "subsystem",
    "subsystems",
];
const TRACE_TERMS: &[&str] = &[
    "caller",
    "callers",
    "called",
    "calls",
    "callee",
    "callees",
    "flow",
    "implementation",
    "trace",
    "used",
    "uses",
    "work",
    "working",
    "works",
];
const CHANGE_TERMS: &[&str] = &[
    "add",
    "change",
    "correct",
    "edit",
    "fix",
    "implement",
    "improve",
    "modify",
    "optimize",
    "reduce",
    "refactor",
    "remove",
    "rename",
    "replace",
];
const SYMBOL_TERMS: &[&str] = &[
    "class",
    "declaration",
    "define",
    "defined",
    "file",
    "find",
    "function",
    "locate",
    "method",
    "symbol",
    "type",
    "where",
];
const NON_DEFINITION_TERMS: &[&str] = &[
    "argument",
    "arguments",
    "field",
    "fields",
    "import",
    "imported",
    "imports",
    "parameter",
    "parameters",
    "properties",
    "property",
    "variable",
    "variables",
];

/// Deterministic coding-task category used to select retrieval and graph policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskIntent {
    /// Locate a declaration, file, or exact code identity.
    SymbolLookup,
    /// Follow implementation and call relationships.
    ImplementationTrace,
    /// Plan or execute a source change and its impact.
    ChangePlanning,
    /// Select verification targets for a change.
    TestSelection,
    /// Diagnose a failure, panic, exception, or incorrect behavior.
    ErrorDiagnosis,
    /// Survey subsystem boundaries and dependency shape.
    ArchitectureSurvey,
    /// Find explanatory documentation or examples.
    DocumentationLookup,
}

/// Semantic direction for graph context attached to a coding-task packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextGraphDirection {
    /// Walk incoming relationships to find callers and dependents.
    Callers,
    /// Walk outgoing relationships to find callees and dependencies.
    Callees,
    /// Walk both incoming and outgoing relationships within the same hard bound.
    Both,
}

impl TaskIntent {
    /// Classify bounded natural-language task text without an LLM or external state.
    #[must_use]
    pub fn classify(task: &str) -> Self {
        let normalized = normalize(task);
        let normalized = normalized.trim();
        let tokens = normalized.split_ascii_whitespace().collect::<Vec<_>>();
        if contains_term(&tokens, ERROR_TERMS) {
            return Self::ErrorDiagnosis;
        }
        if asks_for_tests(&tokens) {
            return Self::TestSelection;
        }
        if contains_term(&tokens, DOCUMENTATION_TERMS) {
            return Self::DocumentationLookup;
        }
        if contains_term(&tokens, ARCHITECTURE_TERMS) {
            return Self::ArchitectureSurvey;
        }
        if is_implementation_question(normalized) {
            return Self::ImplementationTrace;
        }
        if contains_term(&tokens, TRACE_TERMS) {
            return Self::ImplementationTrace;
        }
        if contains_term(&tokens, CHANGE_TERMS) {
            return Self::ChangePlanning;
        }
        if contains_term(&tokens, SYMBOL_TERMS) {
            return Self::SymbolLookup;
        }
        if contains_term(&tokens, TEST_TERMS) {
            return Self::TestSelection;
        }
        if contains_term(&tokens, &["regression"]) {
            return Self::ErrorDiagnosis;
        }
        Self::ChangePlanning
    }

    /// Select a deterministic result preference while honoring explicit auxiliary-code tasks.
    #[must_use]
    pub fn retrieval_preference(self, task: &str) -> RetrievalPreference {
        let normalized = normalize(task);
        let tokens = normalized.split_ascii_whitespace().collect::<Vec<_>>();
        let explicitly_auxiliary = contains_term(
            &tokens,
            &[
                "bench",
                "benches",
                "benchmark",
                "benchmarks",
                "example",
                "examples",
                "fixture",
                "fixtures",
                "test",
                "tests",
            ],
        ) || contains_term(&tokens, NON_DEFINITION_TERMS);
        if explicitly_auxiliary {
            return RetrievalPreference::Neutral;
        }
        match self {
            Self::SymbolLookup
            | Self::ImplementationTrace
            | Self::ChangePlanning
            | Self::ErrorDiagnosis
            | Self::ArchitectureSurvey => RetrievalPreference::ProductionDefinitions,
            Self::TestSelection | Self::DocumentationLookup => RetrievalPreference::Neutral,
        }
    }
}

impl ContextGraphDirection {
    pub(crate) fn classify(task: &str, intent: TaskIntent) -> Option<Self> {
        match intent {
            TaskIntent::ImplementationTrace => Some(trace_direction(task)),
            TaskIntent::ArchitectureSurvey => Some(Self::Both),
            TaskIntent::ChangePlanning | TaskIntent::TestSelection | TaskIntent::ErrorDiagnosis => {
                Some(Self::Callers)
            }
            TaskIntent::SymbolLookup | TaskIntent::DocumentationLookup => None,
        }
    }
}

fn trace_direction(task: &str) -> ContextGraphDirection {
    let normalized = normalize(task);
    let normalized = normalized.trim();
    let tokens = normalized.split_ascii_whitespace().collect::<Vec<_>>();
    let calls_target = normalized.starts_with("what calls ")
        || normalized.starts_with("who calls ")
        || normalized.contains(" that call ");
    let passive_usage =
        contains_term(&tokens, &["where"]) && contains_term(&tokens, &["called", "used"]);
    let includes_incoming = contains_term(&tokens, &["caller", "callers"])
        || normalized.contains("used by")
        || normalized.contains("called by")
        || calls_target
        || passive_usage;
    let includes_outgoing = contains_term(&tokens, &["callee", "callees", "uses"])
        || (contains_term(&tokens, &["calls"]) && !calls_target);
    match (includes_incoming, includes_outgoing) {
        (true, true) => ContextGraphDirection::Both,
        (true, false) => ContextGraphDirection::Callers,
        (false, true | false) => ContextGraphDirection::Callees,
    }
}

fn contains_term(tokens: &[&str], candidates: &[&str]) -> bool {
    tokens
        .iter()
        .any(|token| candidates.iter().any(|candidate| token == candidate))
}

fn asks_for_tests(tokens: &[&str]) -> bool {
    tokens.array_windows().any(|&[first, second]| {
        matches!(first, "which" | "what" | "select") && matches!(second, "test" | "tests")
    })
}

fn is_implementation_question(task: &str) -> bool {
    ["how are ", "how does ", "how is "]
        .iter()
        .any(|prefix| task.starts_with(prefix))
}

fn normalize(task: &str) -> String {
    task.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_agent_tasks_route_to_stable_intents() {
        let cases = [
            ("where is ProjectRuntime defined?", TaskIntent::SymbolLookup),
            (
                "trace callers through the authentication flow",
                TaskIntent::ImplementationTrace,
            ),
            (
                "refactor the database connection lifecycle",
                TaskIntent::ChangePlanning,
            ),
            (
                "which tests verify lease takeover?",
                TaskIntent::TestSelection,
            ),
            (
                "diagnose the panic when a lease expires",
                TaskIntent::ErrorDiagnosis,
            ),
            (
                "explain subsystem dependencies and architecture",
                TaskIntent::ArchitectureSurvey,
            ),
            (
                "find the migration guide and examples",
                TaskIntent::DocumentationLookup,
            ),
        ];
        for (task, expected) in cases {
            assert_eq!(TaskIntent::classify(task), expected, "task: {task}");
        }
    }

    #[test]
    fn failure_and_verification_terms_take_priority_over_generic_change_words() {
        assert_eq!(
            TaskIntent::classify("fix the failing test"),
            TaskIntent::ErrorDiagnosis
        );
        assert_eq!(
            TaskIntent::classify("change which tests verify the parser"),
            TaskIntent::TestSelection
        );
        assert_eq!(
            TaskIntent::classify("improve the thing"),
            TaskIntent::ChangePlanning
        );
    }

    #[test]
    fn implementation_questions_and_change_regressions_do_not_route_as_failures() {
        for task in [
            "How are generation-local BM25 tables checked before a search runs?",
            "How does freshness decide whether a live checkout matches the current generation?",
        ] {
            assert_eq!(
                TaskIntent::classify(task),
                TaskIntent::ImplementationTrace,
                "task: {task}",
            );
        }
        assert_eq!(
            TaskIntent::classify(
                "Improve retrieval quality with regression coverage and compact low-token output",
            ),
            TaskIntent::ChangePlanning,
        );
        assert_eq!(
            TaskIntent::classify("diagnose the retrieval regression"),
            TaskIntent::ErrorDiagnosis,
        );
        assert_eq!(
            TaskIntent::classify("How do retries work?"),
            TaskIntent::ImplementationTrace,
        );
        assert_eq!(
            TaskIntent::classify("How do I improve retry handling?"),
            TaskIntent::ChangePlanning,
        );
        assert_eq!(
            TaskIntent::ImplementationTrace.retrieval_preference(
                "How are generation-local BM25 tables checked before a search runs?",
            ),
            RetrievalPreference::ProductionDefinitions,
        );
        assert_eq!(
            TaskIntent::ChangePlanning.retrieval_preference("change the parser test fixture"),
            RetrievalPreference::Neutral,
        );
        assert_eq!(
            TaskIntent::SymbolLookup.retrieval_preference(
                "Where does similar_current_symbols decode semantic neighbors?",
            ),
            RetrievalPreference::ProductionDefinitions,
        );
        for task in [
            "find the request parameter",
            "locate the imported symbol",
            "where is this struct field",
        ] {
            assert_eq!(
                TaskIntent::SymbolLookup.retrieval_preference(task),
                RetrievalPreference::Neutral,
                "task: {task}",
            );
        }
        assert_eq!(
            TaskIntent::ErrorDiagnosis
                .retrieval_preference("diagnose semantic neighbor decoding failure"),
            RetrievalPreference::ProductionDefinitions,
        );
    }

    #[test]
    fn graph_direction_distinguishes_callers_callees_and_architecture() {
        assert_eq!(
            ContextGraphDirection::classify(
                "trace callers of publish_generation",
                TaskIntent::ImplementationTrace,
            ),
            Some(ContextGraphDirection::Callers)
        );
        assert_eq!(
            ContextGraphDirection::classify(
                "trace what publish_generation calls",
                TaskIntent::ImplementationTrace,
            ),
            Some(ContextGraphDirection::Callees)
        );
        assert_eq!(
            ContextGraphDirection::classify(
                "what calls publish_generation",
                TaskIntent::ImplementationTrace,
            ),
            Some(ContextGraphDirection::Callers)
        );
        assert_eq!(
            ContextGraphDirection::classify(
                "where is publish_generation used",
                TaskIntent::ImplementationTrace,
            ),
            Some(ContextGraphDirection::Callers)
        );
        assert_eq!(
            ContextGraphDirection::classify(
                "show callers and callees",
                TaskIntent::ImplementationTrace,
            ),
            Some(ContextGraphDirection::Both)
        );
        assert_eq!(
            ContextGraphDirection::classify("map architecture", TaskIntent::ArchitectureSurvey),
            Some(ContextGraphDirection::Both)
        );
        assert_eq!(
            ContextGraphDirection::classify("find symbol", TaskIntent::SymbolLookup),
            None
        );
    }
}
