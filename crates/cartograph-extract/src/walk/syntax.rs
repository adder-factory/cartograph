use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cartograph_domain::{
    ContentDigest, SourceLanguage, SourcePosition, SourceSpan, SymbolKind, Visibility,
};
use tree_sitter::{Node, TreeCursor};

use crate::{
    CloneTokenCount, CloneTokenProfile, DiagnosticCode, ExtractError, ExtractionDiagnostic,
    SymbolHealthMetrics, budget::ensure_fact_string_length, model::CloneTokenProfileInput,
};

const MAX_DIAGNOSTICS: usize = 32;
const MAX_BODY_SEARCH_BYTES: usize = 16 * 1024;
const BODY_SEARCH_PREFIX_BYTES: usize = MAX_BODY_SEARCH_BYTES / 2;
const HASH_CHUNK_BYTES: usize = 64 * 1024;
const TEXT_POLL_STRIDE: usize = 4096;
const MAX_DOC_COMMENT_NODES: usize = 1024;
const HEALTH_POLL_STRIDE: usize = 1_024;
const MAX_HEALTH_AST_NODES: usize = 1_000_000;
const MAX_CLONE_PROFILE_DISTINCT_TOKENS: usize = 32 * 1_024;
const MAX_CLONE_PROFILE_TOTAL_TOKENS: u32 = 1_000_000;
const SIGNAL_CATEGORY_CREDENTIAL: u16 = 1 << 0;
const SIGNAL_CATEGORY_SIGNED_CLAIM: u16 = 1 << 1;
const SIGNAL_CATEGORY_LOGIN_MATERIAL: u16 = 1 << 2;
const SIGNAL_CATEGORY_CRYPTOGRAPHY: u16 = 1 << 3;
const SIGNAL_CATEGORY_CLOUD: u16 = 1 << 4;
const SIGNAL_CATEGORY_ENVIRONMENT: u16 = 1 << 5;
const SIGNAL_CATEGORY_PERSONAL_DATA: u16 = 1 << 6;
const SIGNAL_CATEGORY_LITERAL: u16 = 1 << 7;
const SIGNAL_CATEGORY_EXPOSURE: u16 = 1 << 8;
const MAX_SENSITIVE_SCORE: u16 = 100;
const IDENTIFIER_SIGNAL_WEIGHT: u16 = 30;
const SIGNED_CLAIM_SIGNAL_WEIGHT: u16 = 30;
const SIGNED_CLAIM_LITERAL_WEIGHT: u16 = 50;
const LOGIN_MATERIAL_SIGNAL_WEIGHT: u16 = 30;
const CRYPTOGRAPHIC_SIGNAL_WEIGHT: u16 = 20;
const CLOUD_SIGNAL_WEIGHT: u16 = 40;
const CLOUD_LITERAL_WEIGHT: u16 = 60;
const ENVIRONMENT_SIGNAL_WEIGHT: u16 = 30;
const PERSONAL_DATA_SIGNAL_WEIGHT: u16 = 20;
const LONG_LITERAL_SIGNAL_WEIGHT: u16 = 20;
const EXPOSURE_SIGNAL_WEIGHT: u16 = 20;
const CREDENTIAL_IDENTIFIER_SUBSTRINGS: &[&str] = &[
    "api_key",
    "api-key",
    "apikey",
    "access_key",
    "accesskey",
    "client_secret",
    "clientsecret",
];
const CREDENTIAL_IDENTIFIER_WORDS: &[&str] = &["secret", "token"];
const SIGNED_CLAIM_SUBSTRINGS: &[&str] = &[
    "verifyjwt",
    "decodejwt",
    "signjwt",
    "jwt.sign",
    "jwt.verify",
    "jwt.decode",
];
const LOGIN_MATERIAL_WORDS: &[&str] = &["password", "passwd", "pwd", "passphrase"];
const CRYPTOGRAPHIC_OPERATION_COMPONENTS: &[&str] =
    &["hmac", "encrypt", "decrypt", "sign", "verify"];
const CRYPTOGRAPHIC_MATERIAL_COMPONENTS: &[&str] = &["secret", "key"];
const CLOUD_IDENTIFIER_SUBSTRINGS: &[&str] = &[
    "aws_secret_access_key",
    "aws_access_key_id",
    "aws-secret-access-key",
    "aws-access-key-id",
];
const ENVIRONMENT_ACCESS_SUBSTRINGS: &[&str] = &["process.env.", "env[", "getenv("];
const PERSONAL_DATA_SUBSTRINGS: &[&str] = &[
    "ssn",
    "social_security",
    "social-security",
    "credit_card",
    "credit-card",
    "date_of_birth",
    "phone_number",
    "email_address",
];
const SENSITIVE_EXPOSURE_SUBSTRINGS: &[&str] = &[
    "console.log",
    "console.info",
    "console.warn",
    "console.error",
    "logger.",
    "log.",
];
const SENSITIVE_EXPOSURE_CALL_WORDS: &[&str] = &["print", "println"];
const INCOMPLETE_COMMENT_MARKERS: &[&str] = &["todo", "fixme", "xxx", "hack"];
const INCOMPLETE_COMMENT_PHRASE: &str = "not implemented";
const INCOMPLETE_SYNTAX_MARKERS: &[&str] = &[
    "not implemented",
    "notimplementederror",
    "unsupportedoperationexception",
];
const INCOMPLETE_MACRO_NAMES: &[&str] = &["todo", "unimplemented"];
const INTEGER_LITERAL_SUFFIXES: &[&str] = &[
    "usize", "isize", "u128", "i128", "u64", "i64", "u32", "i32", "u16", "i16", "u8", "i8", "ull",
    "llu", "ul", "lu", "ll", "u", "l",
];
const DECIMAL_LITERAL_SUFFIXES: &[&str] = &[
    "usize", "isize", "u128", "i128", "u64", "i64", "u32", "i32", "u16", "i16", "u8", "i8", "f32",
    "f64", "ull", "llu", "ul", "lu", "ll", "u", "l", "f", "d", "m", "n",
];
const HEX_RADIX: u32 = 16;
const OCTAL_RADIX: u32 = 8;
const BINARY_RADIX: u32 = 2;
const GO_NON_MAGIC_INTEGERS: &[u128] = &[7, 24, 30, 60, 365, 1_000, 1_024, 86_400];

#[derive(Clone, Copy)]
struct SensitiveMetricInput<'tree, 'source> {
    declaration: Node<'tree>,
    body: Option<Node<'tree>>,
    symbol_kind: SymbolKind,
    symbol_name: &'source str,
    signature: Option<&'source str>,
    docstring: Option<&'source str>,
    source: &'source str,
}

#[derive(Clone, Copy)]
struct ParameterMetricInput<'tree, 'source> {
    declaration: Node<'tree>,
    symbol_kind: SymbolKind,
    language: SourceLanguage,
    source: &'source str,
}

#[derive(Default)]
struct SensitiveBodyEvidence<'source> {
    code_fields: Vec<&'source str>,
    literal_fields: Vec<&'source str>,
    boundary: SensitiveBoundaryEvidence,
}

#[derive(Default)]
struct SensitiveBodyScan<'source> {
    excluded_ranges: Vec<(usize, usize)>,
    literal_fields: Vec<&'source str>,
    boundary: SensitiveBoundaryEvidence,
}

#[derive(Clone, Copy, Default)]
struct SensitiveBoundaryEvidence {
    environment_secret: bool,
    exposed_sensitive_material: bool,
}

#[derive(Default)]
struct SensitiveScore {
    value: u16,
    signal_mask: u16,
}

#[derive(Clone, Copy)]
struct SensitiveSignal {
    matched: bool,
    category: u16,
    weight: u16,
}

#[derive(Clone, Copy)]
struct HealthWalkInput<'tree, 'source> {
    body: Node<'tree>,
    language: SourceLanguage,
    async_symbol: bool,
    source: &'source str,
}

#[derive(Clone, Copy)]
struct HealthNodeInput<'tree, 'source> {
    node: Node<'tree>,
    nesting: u16,
    root: bool,
    source: &'source str,
    language: SourceLanguage,
}

#[derive(Clone, Copy)]
struct LiteralHealthInput<'tree, 'source> {
    node: Node<'tree>,
    source: &'source str,
    language: SourceLanguage,
}

#[derive(Clone, Copy)]
struct ControlHealthInput<'tree, 'source> {
    node: Node<'tree>,
    nesting: u16,
    source: &'source str,
}

#[derive(Clone, Copy)]
struct TextMetricInput<'source> {
    raw: &'source str,
    language: SourceLanguage,
    async_symbol: bool,
}

struct CodeLineState {
    depth: usize,
    static_jsx_by_depth: Vec<bool>,
    previous_row: Option<usize>,
    lines: u32,
}

impl CodeLineState {
    fn new() -> Self {
        Self {
            depth: 0,
            static_jsx_by_depth: vec![false],
            previous_row: None,
            lines: 0,
        }
    }

    fn record(&mut self, node: Node<'_>, static_jsx: bool) -> bool {
        let comment = is_comment_node(node.kind());
        if !comment && !static_jsx {
            let row = node.start_position().row;
            if self.previous_row != Some(row) {
                self.lines = self.lines.saturating_add(1);
                self.previous_row = Some(row);
            }
        }
        comment
    }

    fn descend(
        &mut self,
        cursor: &mut TreeCursor<'_>,
        child_static_jsx: bool,
        descend: bool,
    ) -> bool {
        if !descend || !cursor.goto_first_child() {
            return false;
        }
        self.depth = self.depth.saturating_add(1);
        if let Some(static_jsx) = self.static_jsx_by_depth.get_mut(self.depth) {
            *static_jsx = child_static_jsx;
        } else {
            self.static_jsx_by_depth.push(child_static_jsx);
        }
        true
    }
}

#[derive(Clone, Copy)]
struct FacadeHealthInput<'tree, 'source> {
    body: Node<'tree>,
    language: SourceLanguage,
    source: &'source str,
}

#[derive(Clone, Copy)]
struct SensitiveBoundaryInput<'source> {
    code_fields: &'source [&'source str],
    literal_fields: &'source [&'source str],
    boundary: SensitiveBoundaryEvidence,
}

#[derive(Clone, Copy)]
pub(crate) struct SymbolHealthInput<'tree, 'source> {
    pub(crate) declaration: Node<'tree>,
    pub(crate) body: Option<Node<'tree>>,
    pub(crate) symbol_kind: SymbolKind,
    pub(crate) symbol_name: &'source str,
    pub(crate) signature: Option<&'source str>,
    pub(crate) docstring: Option<&'source str>,
    pub(crate) language: SourceLanguage,
    pub(crate) async_symbol: bool,
    pub(crate) source: &'source str,
}

pub(crate) fn symbol_health_metrics(
    input: SymbolHealthInput<'_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<SymbolHealthMetrics, ExtractError> {
    let SymbolHealthInput {
        declaration,
        body,
        symbol_kind,
        symbol_name,
        signature,
        docstring,
        language,
        async_symbol,
        source,
    } = input;
    let mut metrics = SymbolHealthMetrics {
        code_lines: code_line_count(declaration, language, cancelled)?,
        parameter_count: parameter_count(ParameterMetricInput {
            declaration,
            symbol_kind,
            language,
            source,
        }),
        cyclomatic: u16::from(body.is_some()),
        ..SymbolHealthMetrics::default()
    };
    populate_sensitive_and_documentation_metrics(
        &mut metrics,
        SensitiveMetricInput {
            declaration,
            body,
            symbol_kind,
            symbol_name,
            signature,
            docstring,
            source,
        },
    );
    let Some(body) = body else {
        return Ok(metrics);
    };
    walk_symbol_health(
        &mut metrics,
        HealthWalkInput {
            body,
            language,
            async_symbol,
            source,
        },
        cancelled,
    )?;
    record_facade_health(
        &mut metrics,
        FacadeHealthInput {
            body,
            language,
            source,
        },
    );
    Ok(metrics)
}

fn code_line_count(
    declaration: Node<'_>,
    language: SourceLanguage,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<u32, ExtractError> {
    let mut cursor = declaration.walk();
    let mut state = CodeLineState::new();
    let mut root = true;
    let mut visited = 0_usize;
    loop {
        let node = cursor.node();
        poll_code_line_walk(&mut visited, cancelled)?;
        let inherited_static_jsx = state
            .static_jsx_by_depth
            .get(state.depth)
            .copied()
            .unwrap_or(false);
        let (static_jsx, child_static_jsx) =
            jsx_line_context(language, node.kind(), inherited_static_jsx);
        let comment = state.record(node, static_jsx);
        let descend = !comment
            && !is_opaque_string_literal(node.kind())
            && (root || !is_callable_node(node.kind()));
        root = false;
        if state.descend(&mut cursor, child_static_jsx, descend) {
            continue;
        }
        if !advance_health_cursor(&mut cursor, &mut state.depth) {
            return Ok(state.lines);
        }
    }
}

fn poll_code_line_walk(
    visited: &mut usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(), ExtractError> {
    *visited = visited.saturating_add(1);
    if *visited > MAX_HEALTH_AST_NODES {
        return Err(ExtractError::OutputLimit);
    }
    if visited.is_multiple_of(HEALTH_POLL_STRIDE) && cancelled() {
        return Err(ExtractError::Cancelled);
    }
    Ok(())
}

fn jsx_line_context(
    language: SourceLanguage,
    node_kind: &str,
    inherited_static_jsx: bool,
) -> (bool, bool) {
    if !is_javascript_language(language) {
        return (false, false);
    }
    if node_kind == "jsx_expression" {
        return (false, false);
    }
    let jsx_scaffolding = is_jsx_scaffolding_node(node_kind);
    let static_jsx = jsx_scaffolding || inherited_static_jsx;
    (static_jsx, static_jsx)
}

fn is_jsx_scaffolding_node(kind: &str) -> bool {
    kind.starts_with("jsx_") && kind != "jsx_expression"
}

fn advance_health_cursor(cursor: &mut TreeCursor<'_>, depth: &mut usize) -> bool {
    loop {
        if cursor.goto_next_sibling() {
            return true;
        }
        if *depth == 0 || !cursor.goto_parent() {
            return false;
        }
        *depth = depth.saturating_sub(1);
    }
}

fn walk_symbol_health(
    metrics: &mut SymbolHealthMetrics,
    input: HealthWalkInput<'_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(), ExtractError> {
    let mut stack = vec![(input.body, 0_u16, true)];
    let mut visited = 0_usize;
    while let Some((node, nesting, root)) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > MAX_HEALTH_AST_NODES {
            return Err(ExtractError::OutputLimit);
        }
        if visited.is_multiple_of(HEALTH_POLL_STRIDE) && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let Some(child_nesting) = inspect_health_node(
            metrics,
            HealthNodeInput {
                node,
                nesting,
                root,
                source: input.source,
                language: input.language,
            },
        ) else {
            continue;
        };
        for child in named_children(node) {
            stack.push((child, child_nesting, false));
        }
    }
    metrics.incomplete_markers = incomplete_marker_count(input.body, input.source);
    let raw = text_for(input.source, input.body);
    populate_text_metrics(
        metrics,
        TextMetricInput {
            raw,
            language: input.language,
            async_symbol: input.async_symbol,
        },
    );
    Ok(())
}

fn inspect_health_node(
    metrics: &mut SymbolHealthMetrics,
    input: HealthNodeInput<'_, '_>,
) -> Option<u16> {
    if !input.root && is_callable_node(input.node.kind()) {
        return None;
    }
    record_literal_health(
        metrics,
        LiteralHealthInput {
            node: input.node,
            source: input.source,
            language: input.language,
        },
    );
    record_dynamic_sql_health(metrics, input);
    let child_nesting = record_control_health(
        metrics,
        ControlHealthInput {
            node: input.node,
            nesting: input.nesting,
            source: input.source,
        },
    );
    metrics.cyclomatic = metrics
        .cyclomatic
        .saturating_add(direct_logical_operator_count(input.node, input.source));
    if input.node.kind() == "catch_clause" && catch_is_empty(input.node, input.source) {
        metrics.empty_catches = metrics.empty_catches.saturating_add(1);
    }
    if is_loop_node(input.node.kind()) {
        record_loop_health(
            metrics,
            LiteralHealthInput {
                node: input.node,
                source: input.source,
                language: input.language,
            },
        );
    }
    Some(child_nesting)
}

fn record_literal_health(metrics: &mut SymbolHealthMetrics, input: LiteralHealthInput<'_, '_>) {
    let node_text = text_for(input.source, input.node);
    let string_literal = is_string_literal(input.node.kind());
    let numeric_literal = is_numeric_literal(input.node.kind());
    let outermost_string_literal = string_literal
        && input
            .node
            .parent()
            .is_none_or(|parent| !is_string_literal(parent.kind()));
    if outermost_string_literal || numeric_literal {
        metrics.literal_bytes = metrics.literal_bytes.saturating_add(
            u32::try_from(
                input
                    .node
                    .end_byte()
                    .saturating_sub(input.node.start_byte()),
            )
            .unwrap_or(u32::MAX),
        );
    }
    if numeric_literal && is_magic_number(node_text, input.language) {
        metrics.magic_numbers = metrics.magic_numbers.saturating_add(1);
    }
    if string_literal && contains_hardcoded_url(node_text) {
        match classify_url_literal(input.node, input.source) {
            UrlLiteralCategory::Request => {
                metrics.hardcoded_urls = metrics.hardcoded_urls.saturating_add(1);
                metrics.hardcoded_url_requests = metrics.hardcoded_url_requests.saturating_add(1);
            }
            UrlLiteralCategory::Configuration => {
                metrics.hardcoded_urls = metrics.hardcoded_urls.saturating_add(1);
                metrics.hardcoded_url_configuration =
                    metrics.hardcoded_url_configuration.saturating_add(1);
            }
            UrlLiteralCategory::Presentation => {
                metrics.hardcoded_url_presentation_abstentions = metrics
                    .hardcoded_url_presentation_abstentions
                    .saturating_add(1);
            }
            UrlLiteralCategory::Validation | UrlLiteralCategory::Data => {}
        }
    }
}

fn record_dynamic_sql_health(metrics: &mut SymbolHealthMetrics, input: HealthNodeInput<'_, '_>) {
    if !is_javascript_language(input.language) {
        return;
    }
    let raw = text_for(input.source, input.node);
    let interpolated_template = input.node.kind() == "template_string"
        && raw.contains("${")
        && contains_sql_statement_shape(raw);
    let concatenated_sql = input.node.kind() == "binary_expression"
        && children(input.node).any(|child| text_for(input.source, child).trim() == "+")
        && descendants_including_root(input.node).any(|candidate| {
            is_string_literal(candidate.kind())
                && candidate
                    .parent()
                    .is_none_or(|parent| !is_string_literal(parent.kind()))
                && contains_sql_statement_shape(text_for(input.source, candidate))
        });
    if interpolated_template || concatenated_sql {
        metrics.sql_string_concatenation = 1;
    }
}

const fn is_javascript_language(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    )
}

fn record_control_health(
    metrics: &mut SymbolHealthMetrics,
    input: ControlHealthInput<'_, '_>,
) -> u16 {
    if !is_control_node(input.node.kind()) {
        return input.nesting;
    }
    metrics.cyclomatic = metrics.cyclomatic.saturating_add(1);
    let depth = if is_else_if_branch(input.node) {
        input.nesting
    } else {
        input.nesting.saturating_add(1)
    };
    metrics.max_nesting = metrics.max_nesting.max(depth);
    if let Some(condition) = condition_node(input.node) {
        metrics.max_conditional_operands = metrics
            .max_conditional_operands
            .max(logical_operator_count(condition, input.source).saturating_add(1));
    }
    depth
}

fn is_else_if_branch(node: Node<'_>) -> bool {
    matches!(node.kind(), "elif_clause" | "else_if_clause")
        || (matches!(node.kind(), "if_expression" | "if_statement")
            && node
                .parent()
                .is_some_and(|parent| parent.kind() == "else_clause"))
}

fn record_loop_health(metrics: &mut SymbolHealthMetrics, input: LiteralHealthInput<'_, '_>) {
    let text = text_for(input.source, input.node);
    if is_javascript_for_of(input.node, input.source, input.language)
        && loop_contains_owned_await(input.node)
    {
        if contains_ascii_case_insensitive(text, "cartograph: serial-await") {
            metrics.serial_await_intent_loops = metrics.serial_await_intent_loops.saturating_add(1);
        } else if loop_has_carried_await_dependency(input.node, input.source) {
            metrics.serial_await_dependency_loops =
                metrics.serial_await_dependency_loops.saturating_add(1);
        } else if loop_has_post_await_exit(input.node) {
            metrics.serial_await_control_flow_loops =
                metrics.serial_await_control_flow_loops.saturating_add(1);
        } else {
            metrics.sequential_await_loops = metrics.sequential_await_loops.saturating_add(1);
        }
    }
    if text.contains("namedChildCount") && text.contains("namedChild(") {
        metrics.accidental_quadratic = metrics.accidental_quadratic.saturating_add(1);
    }
}

fn is_javascript_for_of(node: Node<'_>, source: &str, language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    ) && node.kind() == "for_in_statement"
        && children(node).any(|child| text_for(source, child) == "of")
        && !children(node).any(|child| text_for(source, child) == "await")
}

fn loop_contains_owned_await(node: Node<'_>) -> bool {
    let mut stack = named_children(node).collect::<Vec<_>>();
    while let Some(descendant) = stack.pop() {
        if is_callable_node(descendant.kind()) || is_loop_node(descendant.kind()) {
            continue;
        }
        if descendant.kind() == "await_expression" {
            return true;
        }
        stack.extend(named_children(descendant));
    }
    false
}

fn loop_has_post_await_exit(node: Node<'_>) -> bool {
    let nodes = owned_loop_nodes(node);
    let first_await = nodes
        .iter()
        .filter(|candidate| candidate.kind() == "await_expression")
        .map(tree_sitter::Node::end_byte)
        .min();
    first_await.is_some_and(|await_end| {
        nodes.iter().any(|candidate| {
            matches!(candidate.kind(), "break_statement" | "return_statement")
                && candidate.start_byte() >= await_end
        })
    })
}

fn loop_has_carried_await_dependency(node: Node<'_>, source: &str) -> bool {
    owned_loop_nodes(node).into_iter().any(|candidate| {
        if !matches!(
            candidate.kind(),
            "assignment_expression" | "augmented_assignment_expression"
        ) {
            return false;
        }
        let Some(left) = candidate.child_by_field_name("left") else {
            return false;
        };
        let Some(right) = candidate.child_by_field_name("right") else {
            return false;
        };
        if !descendants_including_root(right).any(|node| node.kind() == "await_expression") {
            return false;
        }
        let left_identifiers = descendants_including_root(left)
            .filter(|node| node.kind() == "identifier")
            .map(|node| text_for(source, node))
            .collect::<BTreeSet<_>>();
        descendants_including_root(right)
            .filter(|node| node.kind() == "identifier")
            .map(|node| text_for(source, node))
            .any(|identifier| left_identifiers.contains(identifier))
    })
}

fn owned_loop_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut nodes = Vec::new();
    let mut stack = named_children(root).collect::<Vec<_>>();
    while let Some(node) = stack.pop() {
        if is_callable_node(node.kind()) || is_loop_node(node.kind()) {
            continue;
        }
        nodes.push(node);
        stack.extend(named_children(node));
    }
    nodes
}

fn record_facade_health(metrics: &mut SymbolHealthMetrics, input: FacadeHealthInput<'_, '_>) {
    if !is_javascript_language(input.language) {
        return;
    }
    let mut nested_callables = Vec::new();
    let mut outer_stack = named_children(input.body).collect::<Vec<_>>();
    let mut has_returned_object = false;
    let mut outer_side_effects = 0_u16;
    while let Some(node) = outer_stack.pop() {
        if is_callable_node(node.kind()) {
            nested_callables.push(node);
            continue;
        }
        if node.kind() == "return_statement"
            && named_children(node).any(|child| child.kind() == "object")
        {
            has_returned_object = true;
        }
        if matches!(
            node.kind(),
            "call_expression" | "new_expression" | "throw_statement" | "update_expression"
        ) {
            outer_side_effects = outer_side_effects.saturating_add(1);
        }
        outer_stack.extend(named_children(node));
    }
    let delegate_count = nested_callables
        .iter()
        .filter(|callable| callable_is_focused_delegate(**callable, input.source))
        .count();
    if has_returned_object
        && nested_callables.len() >= 3
        && delegate_count == nested_callables.len()
        && outer_side_effects == 0
    {
        metrics.facade_factory = true;
        metrics.facade_factory_delegates = u16::try_from(delegate_count).unwrap_or(u16::MAX);
    }
}

fn callable_is_focused_delegate(callable: Node<'_>, _source: &str) -> bool {
    let mut calls = 0_u16;
    let mut stack = named_children(callable).collect::<Vec<_>>();
    while let Some(node) = stack.pop() {
        if node != callable && is_callable_node(node.kind()) {
            continue;
        }
        if node.kind() == "call_expression" {
            calls = calls.saturating_add(1);
        }
        if is_control_node(node.kind())
            || matches!(
                node.kind(),
                "assignment_expression"
                    | "augmented_assignment_expression"
                    | "throw_statement"
                    | "update_expression"
            )
        {
            return false;
        }
        stack.extend(named_children(node));
    }
    calls == 1
}

fn incomplete_marker_count(body: Node<'_>, source: &str) -> u16 {
    let mut count = 0_u16;
    let mut stack = named_children(body).collect::<Vec<_>>();
    while let Some(node) = stack.pop() {
        if is_callable_node(node.kind()) {
            continue;
        }
        let text = text_for(source, node);
        if is_comment_node(node.kind()) {
            count = INCOMPLETE_COMMENT_MARKERS
                .iter()
                .fold(count, |total, marker| {
                    total.saturating_add(count_word_ascii_case_insensitive(text, marker))
                });
            count = count.saturating_add(u16::from(contains_ascii_case_insensitive(
                text,
                INCOMPLETE_COMMENT_PHRASE,
            )));
            continue;
        }
        if contains_incomplete_syntax_marker(node, source) {
            count = count.saturating_add(1);
            continue;
        }
        stack.extend(named_children(node));
    }
    count
}

fn contains_incomplete_syntax_marker(node: Node<'_>, source: &str) -> bool {
    match node.kind() {
        "macro_invocation" => node
            .child_by_field_name("macro")
            .or_else(|| node.named_child(0))
            .is_some_and(|target| {
                fields_contain_identifier_component(
                    &[text_for(source, target)],
                    INCOMPLETE_MACRO_NAMES,
                )
            }),
        "call" | "call_expression" => node
            .child_by_field_name("function")
            .or_else(|| node.named_child(0))
            .is_some_and(|target| {
                INCOMPLETE_SYNTAX_MARKERS
                    .iter()
                    .any(|marker| contains_ascii_case_insensitive(text_for(source, target), marker))
            }),
        "raise_statement" | "throw_statement" => INCOMPLETE_SYNTAX_MARKERS
            .iter()
            .any(|marker| contains_ascii_case_insensitive(text_for(source, node), marker)),
        _ => false,
    }
}

fn parameter_count(input: ParameterMetricInput<'_, '_>) -> u16 {
    input
        .declaration
        .child_by_field_name("parameters")
        .map_or_else(
            || u16::from(input.declaration.child_by_field_name("parameter").is_some()),
            |parameters| {
                u16::try_from(
                    named_children(parameters)
                        .enumerate()
                        .filter(|(index, parameter)| {
                            !is_implicit_receiver_parameter(*parameter, *index, input)
                        })
                        .count(),
                )
                .unwrap_or(u16::MAX)
            },
        )
}

fn is_implicit_receiver_parameter(
    parameter: Node<'_>,
    index: usize,
    input: ParameterMetricInput<'_, '_>,
) -> bool {
    if parameter.kind() == "self_parameter" {
        return true;
    }
    let Some(name) = leading_parameter_identifier(text_for(input.source, parameter)) else {
        return false;
    };
    matches!(
        input.language,
        SourceLanguage::TypeScript | SourceLanguage::Tsx
    ) && name == "this"
        || (input.language == SourceLanguage::Python
            && input.symbol_kind == SymbolKind::Method
            && index == 0
            && matches!(name, "self" | "cls"))
}

fn leading_parameter_identifier(parameter: &str) -> Option<&str> {
    parameter
        .trim_start_matches(|character: char| character.is_whitespace() || character == '*')
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .next()
        .filter(|name| !name.is_empty())
}

fn populate_sensitive_and_documentation_metrics(
    metrics: &mut SymbolHealthMetrics,
    input: SensitiveMetricInput<'_, '_>,
) {
    let declaration_text = text_for(input.source, input.declaration);
    let body_text = input
        .body
        .map_or(declaration_text, |body| text_for(input.source, body));
    let mut code_fields = vec![input.symbol_name];
    if let Some(signature) = input.signature {
        code_fields.push(signature);
    }
    let evidence = input
        .body
        .map_or_else(SensitiveBodyEvidence::default, |body| {
            sensitive_body_evidence(body, input.source)
        });
    code_fields.extend(evidence.code_fields);
    let (mut score, signal_mask, actionable) =
        sensitive_material_score(&code_fields, &evidence.literal_fields, evidence.boundary);
    if is_test_symbol_name(input.symbol_name) {
        score /= 2;
    }
    metrics.secrets_score = score.min(MAX_SENSITIVE_SCORE);
    metrics.secrets_signal_mask = signal_mask;
    metrics.secrets_actionable = actionable;
    if input.symbol_kind == SymbolKind::Constant
        && let Some(docstring) = input.docstring
        && !looks_like_regex_literal(body_text)
    {
        let doc_claims = documented_numeric_claims(docstring);
        let value_claims = numeric_claims(body_text);
        if !doc_claims.is_empty()
            && !value_claims.is_empty()
            && doc_claims.is_disjoint(&value_claims)
        {
            metrics.stale_doc_numbers = u16::try_from(doc_claims.len()).unwrap_or(u16::MAX);
        }
    }
}

fn sensitive_body_evidence<'source>(
    body: Node<'_>,
    source: &'source str,
) -> SensitiveBodyEvidence<'source> {
    let SensitiveBodyScan {
        mut excluded_ranges,
        literal_fields,
        boundary,
    } = scan_sensitive_body(body, source);
    excluded_ranges.sort_unstable();
    let code_fields = code_fields_outside_ranges(body, source, excluded_ranges);
    SensitiveBodyEvidence {
        code_fields,
        literal_fields,
        boundary,
    }
}

fn scan_sensitive_body<'source>(
    body: Node<'_>,
    source: &'source str,
) -> SensitiveBodyScan<'source> {
    let mut scan = SensitiveBodyScan::default();
    let mut stack = named_children(body).collect::<Vec<_>>();
    while let Some(node) = stack.pop() {
        if is_callable_node(node.kind())
            || is_comment_node(node.kind())
            || is_presentation_only_node(node.kind())
        {
            scan.excluded_ranges
                .push((node.start_byte(), node.end_byte()));
            continue;
        }
        let node_text = text_for(source, node);
        if is_environment_access_boundary(node.kind()) {
            scan.boundary.environment_secret |= text_contains_environment_secret(node_text);
        }
        if is_sensitive_exposure_boundary(node.kind()) {
            scan.boundary.exposed_sensitive_material |= text_contains_sensitive_exposure(node_text);
        }
        if is_opaque_string_literal(node.kind()) {
            if let Some(literal) = source.get(node.start_byte()..node.end_byte()) {
                scan.literal_fields.push(literal);
            }
            scan.excluded_ranges
                .push((node.start_byte(), node.end_byte()));
            continue;
        }
        stack.extend(named_children(node));
    }
    scan
}

fn code_fields_outside_ranges<'source>(
    body: Node<'_>,
    source: &'source str,
    excluded_ranges: Vec<(usize, usize)>,
) -> Vec<&'source str> {
    let mut code_fields = Vec::new();
    let mut cursor = body.start_byte();
    for (start, end) in excluded_ranges {
        let start = start.max(cursor).min(body.end_byte());
        let end = end.max(start).min(body.end_byte());
        if cursor < start
            && let Some(code) = source.get(cursor..start)
        {
            code_fields.push(code);
        }
        cursor = cursor.max(end);
    }
    if cursor < body.end_byte()
        && let Some(code) = source.get(cursor..body.end_byte())
    {
        code_fields.push(code);
    }
    code_fields
}

fn sensitive_material_score(
    code_fields: &[&str],
    literal_fields: &[&str],
    boundary: SensitiveBoundaryEvidence,
) -> (u16, u16, bool) {
    let mut score = SensitiveScore::default();
    add_sensitive_signal(
        &mut score,
        SensitiveSignal {
            matched: contains_identifier_signal(code_fields),
            category: SIGNAL_CATEGORY_CREDENTIAL,
            weight: IDENTIFIER_SIGNAL_WEIGHT,
        },
    );
    let signed_claim_literal = literal_fields
        .iter()
        .any(|field| contains_jwt_literal(field));
    add_sensitive_signal(
        &mut score,
        SensitiveSignal {
            matched: signed_claim_literal
                || fields_contain_substring(code_fields, SIGNED_CLAIM_SUBSTRINGS),
            category: SIGNAL_CATEGORY_SIGNED_CLAIM,
            weight: if signed_claim_literal {
                SIGNED_CLAIM_LITERAL_WEIGHT
            } else {
                SIGNED_CLAIM_SIGNAL_WEIGHT
            },
        },
    );
    add_sensitive_signal(
        &mut score,
        SensitiveSignal {
            matched: fields_contain_identifier_component(code_fields, LOGIN_MATERIAL_WORDS),
            category: SIGNAL_CATEGORY_LOGIN_MATERIAL,
            weight: LOGIN_MATERIAL_SIGNAL_WEIGHT,
        },
    );
    add_sensitive_signal(
        &mut score,
        SensitiveSignal {
            matched: fields_contain_identifier_component(
                code_fields,
                CRYPTOGRAPHIC_OPERATION_COMPONENTS,
            ) && fields_contain_identifier_component(
                code_fields,
                CRYPTOGRAPHIC_MATERIAL_COMPONENTS,
            ),
            category: SIGNAL_CATEGORY_CRYPTOGRAPHY,
            weight: CRYPTOGRAPHIC_SIGNAL_WEIGHT,
        },
    );
    let cloud_literal = literal_fields
        .iter()
        .any(|field| contains_aws_access_key_literal(field));
    add_sensitive_signal(
        &mut score,
        SensitiveSignal {
            matched: cloud_literal
                || fields_contain_substring(code_fields, CLOUD_IDENTIFIER_SUBSTRINGS),
            category: SIGNAL_CATEGORY_CLOUD,
            weight: if cloud_literal {
                CLOUD_LITERAL_WEIGHT
            } else {
                CLOUD_SIGNAL_WEIGHT
            },
        },
    );
    let actionable_boundary = add_sensitive_boundary_signals(
        &mut score,
        SensitiveBoundaryInput {
            code_fields,
            literal_fields,
            boundary,
        },
    );
    let actionable = signed_claim_literal || cloud_literal || actionable_boundary;
    (
        score.value.min(MAX_SENSITIVE_SCORE),
        score.signal_mask,
        actionable,
    )
}

fn add_sensitive_boundary_signals(
    score: &mut SensitiveScore,
    input: SensitiveBoundaryInput<'_>,
) -> bool {
    add_sensitive_signal(
        score,
        SensitiveSignal {
            matched: input.boundary.environment_secret,
            category: SIGNAL_CATEGORY_ENVIRONMENT,
            weight: ENVIRONMENT_SIGNAL_WEIGHT,
        },
    );
    add_sensitive_signal(
        score,
        SensitiveSignal {
            matched: fields_contain_identifier_component(
                input.code_fields,
                PERSONAL_DATA_SUBSTRINGS,
            ),
            category: SIGNAL_CATEGORY_PERSONAL_DATA,
            weight: PERSONAL_DATA_SIGNAL_WEIGHT,
        },
    );
    let long_token_literal = input
        .literal_fields
        .iter()
        .any(|field| contains_long_token_literal(field));
    add_sensitive_signal(
        score,
        SensitiveSignal {
            matched: long_token_literal,
            category: SIGNAL_CATEGORY_LITERAL,
            weight: LONG_LITERAL_SIGNAL_WEIGHT,
        },
    );
    add_sensitive_signal(
        score,
        SensitiveSignal {
            matched: input.boundary.exposed_sensitive_material,
            category: SIGNAL_CATEGORY_EXPOSURE,
            weight: EXPOSURE_SIGNAL_WEIGHT,
        },
    );
    input.boundary.environment_secret
        || long_token_literal
        || input.boundary.exposed_sensitive_material
}

fn is_environment_access_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "attribute"
            | "call"
            | "call_expression"
            | "field_expression"
            | "index_expression"
            | "member_expression"
            | "subscript_expression"
    )
}

fn is_sensitive_exposure_boundary(kind: &str) -> bool {
    matches!(kind, "call" | "call_expression" | "macro_invocation")
}

fn text_contains_environment_secret(text: &str) -> bool {
    fields_contain_substring(&[text], ENVIRONMENT_ACCESS_SUBSTRINGS)
        && text_contains_sensitive_material(text)
}

fn text_contains_sensitive_exposure(text: &str) -> bool {
    (fields_contain_substring(&[text], SENSITIVE_EXPOSURE_SUBSTRINGS)
        || fields_contain_identifier_component(&[text], SENSITIVE_EXPOSURE_CALL_WORDS))
        && text_contains_sensitive_material(text)
}

fn text_contains_sensitive_material(text: &str) -> bool {
    let field = [text];
    contains_identifier_signal(&field)
        || fields_contain_identifier_component(&field, LOGIN_MATERIAL_WORDS)
        || fields_contain_identifier_component(&field, PERSONAL_DATA_SUBSTRINGS)
        || (fields_contain_identifier_component(&field, CRYPTOGRAPHIC_OPERATION_COMPONENTS)
            && fields_contain_identifier_component(&field, CRYPTOGRAPHIC_MATERIAL_COMPONENTS))
}

fn contains_identifier_signal(fields: &[&str]) -> bool {
    fields_contain_substring(fields, CREDENTIAL_IDENTIFIER_SUBSTRINGS)
        || fields_contain_identifier_component(fields, CREDENTIAL_IDENTIFIER_WORDS)
}

fn fields_contain_substring(fields: &[&str], terms: &[&str]) -> bool {
    fields.iter().any(|field| {
        terms
            .iter()
            .any(|term| contains_ascii_case_insensitive(field, term))
    })
}

fn fields_contain_identifier_component(fields: &[&str], terms: &[&str]) -> bool {
    fields.iter().any(|field| {
        terms
            .iter()
            .any(|term| contains_identifier_component_ascii_case_insensitive(field, term))
    })
}

fn contains_identifier_component_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .enumerate()
        .any(|(index, window)| {
            let before = index
                .checked_sub(1)
                .and_then(|previous| haystack.as_bytes().get(previous));
            let after = haystack.as_bytes().get(index.saturating_add(needle.len()));
            window.eq_ignore_ascii_case(needle.as_bytes())
                && before.is_none_or(|byte| {
                    !byte.is_ascii_alphanumeric()
                        || matches!(byte, b'_' | b'$')
                        || window.first().is_some_and(u8::is_ascii_uppercase)
                })
                && after.is_none_or(|byte| {
                    !byte.is_ascii_alphanumeric()
                        || matches!(byte, b'_' | b'$')
                        || byte.is_ascii_uppercase()
                })
        })
}

fn add_sensitive_signal(score: &mut SensitiveScore, signal: SensitiveSignal) {
    if signal.matched {
        score.value = score.value.saturating_add(signal.weight);
        score.signal_mask |= signal.category;
    }
}

fn count_word_ascii_case_insensitive(haystack: &str, needle: &str) -> u16 {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .enumerate()
        .fold(0_u16, |count, (index, window)| {
            count.saturating_add(u16::from(
                window.eq_ignore_ascii_case(needle.as_bytes())
                    && index
                        .checked_sub(1)
                        .and_then(|previous| haystack.as_bytes().get(previous))
                        .is_none_or(|byte| !is_identifier_byte(*byte))
                    && haystack
                        .as_bytes()
                        .get(index.saturating_add(needle.len()))
                        .is_none_or(|byte| !is_identifier_byte(*byte)),
            ))
        })
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn contains_jwt_literal(text: &str) -> bool {
    text.as_bytes()
        .array_windows()
        .any(|window| window == b"eyJ")
}

fn contains_aws_access_key_literal(text: &str) -> bool {
    text.as_bytes().windows(20).any(|window| {
        window.starts_with(b"AKIA")
            && window[4..]
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    })
}

fn contains_long_token_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0_usize;
    while index < bytes.len() {
        let quote = bytes[index];
        if !matches!(quote, b'\'' | b'"' | b'`') {
            index = index.saturating_add(1);
            continue;
        }
        let start = index.saturating_add(1);
        index = start;
        while index < bytes.len() && bytes[index] != quote {
            if bytes[index] == b'\\' {
                index = index.saturating_add(1);
            }
            index = index.saturating_add(1);
        }
        let Some(token) = text.get(start..index) else {
            continue;
        };
        let mime = [
            "application/",
            "audio/",
            "font/",
            "image/",
            "message/",
            "model/",
            "multipart/",
            "text/",
            "video/",
        ]
        .iter()
        .any(|prefix| token.starts_with(prefix));
        if !mime
            && token.len() >= 32
            && token.bytes().any(|byte| byte.is_ascii_digit())
            && token.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'+' | b'=' | b'-')
            })
        {
            return true;
        }
        index = index.saturating_add(1);
    }
    false
}

fn is_test_symbol_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test")
        || lower.starts_with("spec")
        || lower.starts_with("should")
        || lower.starts_with("describe")
        || lower.ends_with("_test")
        || lower.ends_with("-test")
        || lower.ends_with("_spec")
        || lower.ends_with("-spec")
}

fn looks_like_regex_literal(body: &str) -> bool {
    let trimmed = body.trim().trim_start_matches('=').trim();
    trimmed.starts_with('/') && !trimmed.starts_with("//") && !trimmed.starts_with("/*")
}

fn numeric_claims(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut claims = BTreeSet::new();
    let mut index = 0_usize;
    while index < bytes.len() {
        let Some(end) = numeric_token_end(bytes, index) else {
            index = index.saturating_add(1);
            continue;
        };
        let start = index;
        index = end;
        if !numeric_claim_is_value(text, start, index) {
            continue;
        }
        if let Some(claim) = canonical_numeric_claim(text, start, index) {
            claims.insert(claim);
        }
    }
    claims
}

fn numeric_token_end(bytes: &[u8], start: usize) -> Option<usize> {
    let starts_number = bytes[start].is_ascii_digit()
        || bytes[start] == b'-'
            && bytes
                .get(start.saturating_add(1))
                .is_some_and(u8::is_ascii_digit);
    if !starts_number {
        return None;
    }
    let mut cursor = start + usize::from(bytes[start] == b'-');
    let mut decimal_seen = false;
    while let Some(byte) = bytes.get(cursor) {
        if byte.is_ascii_digit() || *byte == b'_' {
            cursor = cursor.saturating_add(1);
            continue;
        }
        let decimal = *byte == b'.'
            && !decimal_seen
            && bytes
                .get(cursor.saturating_add(1))
                .is_some_and(u8::is_ascii_digit);
        if !decimal {
            break;
        }
        decimal_seen = true;
        cursor = cursor.saturating_add(1);
    }
    Some(cursor)
}

fn canonical_numeric_claim(text: &str, start: usize, end: usize) -> Option<String> {
    let normalized = text.get(start..end)?.replace('_', "");
    let number = normalized.parse::<f64>().ok()?;
    (number.is_finite() && !(1900.0..=2100.0).contains(&number)).then(|| canonical_number(number))
}

fn documented_numeric_claims(text: &str) -> BTreeSet<String> {
    let explicit_word = [
        "default",
        "limit",
        "minimum",
        "maximum",
        "threshold",
        "value",
        "count",
        "size",
        "capacity",
        "timeout",
        "retry",
        "retries",
    ]
    .iter()
    .any(|cue| count_word_ascii_case_insensitive(text, cue) > 0);
    let explicit_phrase = [
        "is set to",
        "must be",
        "equal to",
        "equals",
        "configured to",
    ]
    .iter()
    .any(|cue| contains_ascii_case_insensitive(text, cue));
    if explicit_word || explicit_phrase {
        numeric_claims(text)
    } else {
        BTreeSet::new()
    }
}

fn numeric_claim_is_value(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    if start > 0 {
        let previous = bytes[start - 1];
        if is_identifier_byte(previous) || matches!(previous, b'#' | b'\'') {
            return false;
        }
    }
    if let Some(next) = bytes.get(end)
        && (next.is_ascii_alphabetic() || matches!(next, b'%' | b'-'))
    {
        return false;
    }
    let prefix = text.get(..start).unwrap_or_default().trim_end();
    let last_word = prefix
        .rsplit(|character: char| !character.is_ascii_alphabetic())
        .find(|word| !word.is_empty())
        .unwrap_or_default();
    if ["rfc", "port", "issue", "pr", "chapter", "section", "figure"]
        .iter()
        .any(|word| last_word.eq_ignore_ascii_case(word))
    {
        return false;
    }
    let tail = text.get(end..).unwrap_or_default().trim_start();
    let unit = tail
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    ![
        "ms",
        "msec",
        "millisecond",
        "milliseconds",
        "s",
        "sec",
        "second",
        "seconds",
        "min",
        "minute",
        "minutes",
        "h",
        "hr",
        "hour",
        "hours",
        "day",
        "days",
        "week",
        "weeks",
        "byte",
        "bytes",
        "kb",
        "kib",
        "mb",
        "mib",
        "gb",
        "gib",
        "tb",
        "tib",
    ]
    .iter()
    .any(|candidate| unit.eq_ignore_ascii_case(candidate))
}

fn canonical_number(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}

fn is_numeric_literal(kind: &str) -> bool {
    matches!(
        kind,
        "number"
            | "number_literal"
            | "numeric_literal"
            | "integer_literal"
            | "float_literal"
            | "int_literal"
            | "decimal_integer_literal"
            | "hex_integer_literal"
            | "octal_integer_literal"
            | "binary_integer_literal"
            | "real_literal"
            | "integer"
            | "float"
    )
}

fn is_magic_number(raw: &str, language: SourceLanguage) -> bool {
    let normalized = raw.trim().replace('_', "").to_ascii_lowercase();
    if normalized.is_empty()
        || !normalized
            .trim_start_matches(['-', '+'])
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_digit)
    {
        return false;
    }
    let unsigned = normalized.trim_start_matches(['-', '+']);
    let normalized_value = strip_numeric_literal_suffix(unsigned);
    if benign_numeric_value(normalized_value, language) {
        return false;
    }
    true
}

fn strip_numeric_literal_suffix(value: &str) -> &str {
    let suffixes = if value.starts_with("0x") {
        INTEGER_LITERAL_SUFFIXES
    } else {
        DECIMAL_LITERAL_SUFFIXES
    };
    suffixes
        .iter()
        .find_map(|suffix| {
            value
                .strip_suffix(suffix)
                .filter(|number| !number.is_empty())
        })
        .unwrap_or(value)
}

fn benign_numeric_value(value: &str, language: SourceLanguage) -> bool {
    let integer = value
        .strip_prefix("0x")
        .map(|digits| (digits, HEX_RADIX))
        .or_else(|| value.strip_prefix("0o").map(|digits| (digits, OCTAL_RADIX)))
        .or_else(|| {
            value
                .strip_prefix("0b")
                .map(|digits| (digits, BINARY_RADIX))
        })
        .and_then(|(digits, radix)| u128::from_str_radix(digits, radix).ok())
        .or_else(|| value.parse::<u128>().ok());
    if let Some(integer) = integer {
        return matches!(integer, 0..=2)
            || (language == SourceLanguage::Go && GO_NON_MAGIC_INTEGERS.contains(&integer));
    }
    value
        .parse::<f64>()
        .is_ok_and(|number| matches!(number, 0.0 | 1.0 | 2.0))
}

fn is_string_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string"
            | "string_literal"
            | "raw_string_literal"
            | "interpreted_string_literal"
            | "template_string"
            | "string_content"
    )
}

fn is_opaque_string_literal(kind: &str) -> bool {
    is_string_literal(kind) && kind != "template_string"
}

fn contains_hardcoded_url(raw: &str) -> bool {
    [
        "http://", "https://", "ws://", "wss://", "ftp://", "s3://", "gs://",
    ]
    .iter()
    .any(|scheme| {
        let mut offset = 0_usize;
        while let Some(relative) = raw.get(offset..).and_then(|tail| tail.find(scheme)) {
            let start = offset.saturating_add(relative);
            let after = start.saturating_add(scheme.len());
            if raw.as_bytes().get(after).is_some_and(|byte| {
                !byte.is_ascii_whitespace()
                    && !matches!(byte, b'\'' | b'"' | b'`' | b')' | b'%' | b'{' | b'$')
            }) && !is_xml_namespace_uri(raw, start)
            {
                return true;
            }
            offset = after;
        }
        false
    })
}

fn is_xml_namespace_uri(raw: &str, start: usize) -> bool {
    let candidate = raw.get(start..).unwrap_or_default();
    let Some((scheme, location)) = candidate.split_once("://") else {
        return false;
    };
    let standard_namespace = matches!(scheme, "http" | "https")
        && [
            "www.w3.org/",
            "schemas.openxmlformats.org/",
            "schemas.microsoft.com/",
        ]
        .iter()
        .any(|prefix| location.starts_with(prefix));
    if !standard_namespace {
        return false;
    }
    let prefix = raw.get(..start).unwrap_or_default();
    let attribute_context = prefix
        .rsplit(['<', '>'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    attribute_context.contains("xmlns")
}

fn is_url_validation_input(node: Node<'_>, source: &str) -> bool {
    let mut current = Some(node);
    for _ in 0..8 {
        let Some(candidate) = current else {
            return false;
        };
        if candidate.kind() == "call_expression"
            && candidate
                .child_by_field_name("function")
                .is_some_and(|function| {
                    let callee = text_for(source, function).trim();
                    callee == "safeParse" || callee.ends_with(".safeParse")
                })
        {
            return true;
        }
        current = candidate.parent();
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UrlLiteralCategory {
    Request,
    Configuration,
    Presentation,
    Validation,
    Data,
}

fn classify_url_literal(node: Node<'_>, source: &str) -> UrlLiteralCategory {
    if is_url_validation_input(node, source) {
        return UrlLiteralCategory::Validation;
    }
    let mut current = Some(node);
    for _ in 0..12 {
        let Some(candidate) = current else {
            break;
        };
        if let Some(category) = url_literal_ancestor_category(candidate, source) {
            return category;
        }
        if candidate != node && is_callable_node(candidate.kind()) {
            break;
        }
        current = candidate.parent();
    }
    UrlLiteralCategory::Data
}

fn url_literal_ancestor_category(node: Node<'_>, source: &str) -> Option<UrlLiteralCategory> {
    if jsx_url_presentation_attribute(node, source) {
        return Some(UrlLiteralCategory::Presentation);
    }
    if node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| request_callee(text_for(source, function)))
    {
        return Some(UrlLiteralCategory::Request);
    }
    let endpoint_name = matches!(node.kind(), "variable_declarator" | "pair")
        && node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("key"))
            .is_some_and(|name| endpoint_configuration_name(text_for(source, name)));
    endpoint_name.then_some(UrlLiteralCategory::Configuration)
}

fn jsx_url_presentation_attribute(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "jsx_attribute" {
        return false;
    }
    let attribute = node
        .child_by_field_name("name")
        .or_else(|| named_children(node).next())
        .map(|name| text_for(source, name).trim())
        .unwrap_or_default();
    matches!(attribute, "placeholder" | "href" | "src")
}

fn request_callee(raw: &str) -> bool {
    let callee = raw.trim().to_ascii_lowercase();
    matches!(
        callee.as_str(),
        "fetch" | "request" | "axios" | "got" | "ky" | "websocket" | "eventsource"
    ) || [
        ".fetch", ".request", ".get", ".post", ".put", ".patch", ".delete", ".connect",
    ]
    .iter()
    .any(|suffix| callee.ends_with(suffix))
}

fn endpoint_configuration_name(raw: &str) -> bool {
    let name = raw
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(char::from)
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "url", "uri", "endpoint", "baseurl", "origin", "webhook", "socket",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

fn is_presentation_only_node(kind: &str) -> bool {
    matches!(kind, "jsx_text" | "jsx_attribute")
}

fn is_callable_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "function_expression"
            | "arrow_function"
            | "method_definition"
            | "function_item"
            | "function_definition"
            | "method_declaration"
            | "lambda"
            | "closure_expression"
    )
}

fn is_control_node(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "if_expression"
            | "elif_clause"
            | "else_if_clause"
            | "unless"
            | "for_statement"
            | "for_in_statement"
            | "for_expression"
            | "while_statement"
            | "while_expression"
            | "do_statement"
            | "catch_clause"
            | "except_clause"
            | "rescue"
            | "case_statement"
            | "switch_case"
            | "match_arm"
            | "when_entry"
            | "conditional_expression"
            | "ternary_expression"
    )
}

fn is_loop_node(kind: &str) -> bool {
    matches!(
        kind,
        "for_statement"
            | "for_in_statement"
            | "for_expression"
            | "while_statement"
            | "while_expression"
            | "do_statement"
    )
}

fn condition_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("condition")
        .or_else(|| node.child_by_field_name("predicate"))
        .or_else(|| node.child_by_field_name("value"))
}

fn logical_operator_count(node: Node<'_>, source: &str) -> u16 {
    descendants_including_root(node).fold(0_u16, |count, node| {
        count.saturating_add(direct_logical_operator_count(node, source))
    })
}

fn direct_logical_operator_count(node: Node<'_>, source: &str) -> u16 {
    if !matches!(
        node.kind(),
        "binary_expression" | "boolean_operator" | "logical_expression"
    ) {
        return 0;
    }
    let mut count = 0_u16;
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if matches!(text_for(source, child).trim(), "&&" | "||" | "and" | "or") {
            count = count.saturating_add(1);
        }
    }
    count
}

fn catch_is_empty(node: Node<'_>, source: &str) -> bool {
    let Some(body) = node.child_by_field_name("body") else {
        return false;
    };
    let mut statements = named_children(body).filter(|child| !is_comment_node(child.kind()));
    let Some(statement) = statements.next() else {
        return true;
    };
    if statements.next().is_some() {
        return false;
    }
    match statement.kind() {
        "pass_statement" => true,
        "return_statement" => named_children(statement).next().is_none(),
        _ => body_is_empty(text_for(source, body)),
    }
}

fn populate_text_metrics(metrics: &mut SymbolHealthMetrics, input: TextMetricInput<'_>) {
    let javascript = matches!(
        input.language,
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    );
    let typescript = matches!(
        input.language,
        SourceLanguage::TypeScript | SourceLanguage::Tsx
    );
    if typescript {
        populate_typescript_metrics(metrics, input.raw);
    }
    if javascript {
        populate_javascript_metrics(metrics, input);
    }
    metrics.empty_body = u16::from(body_is_empty(input.raw));
}

fn populate_typescript_metrics(metrics: &mut SymbolHealthMetrics, raw: &str) {
    metrics.ts_any_casts = count_ascii_case_insensitive(raw, " as any")
        .saturating_add(count_ascii_case_insensitive(raw, " as unknown as "));
    metrics.ts_suppressions = count_ascii_case_insensitive(raw, "@ts-ignore")
        .saturating_add(count_ascii_case_insensitive(raw, "@ts-expect-error"));
}

fn populate_javascript_metrics(metrics: &mut SymbolHealthMetrics, input: TextMetricInput<'_>) {
    metrics.debug_logs =
        count_javascript_signals(input.raw, &["console.log(", "console.debug(", "debugger;"]);
    metrics.dynamic_eval = count_javascript_signals(input.raw, &["eval(", "new function("]);
    metrics.insecure_hash = count_javascript_signals(
        input.raw,
        &[
            "createhash('md5'",
            "createhash(\"md5\"",
            "createhash('sha1'",
            "createhash(\"sha1\"",
        ],
    );
    populate_javascript_safety_metrics(metrics, input.raw);
    if input.async_symbol {
        metrics.sync_io_in_async = count_javascript_signals(
            input.raw,
            &[
                "readfilesync(",
                "writefilesync(",
                "appendfilesync(",
                "execsync(",
                "execfilesync(",
                "spawnsync(",
            ],
        );
    }
}

fn count_javascript_signals(raw: &str, signals: &[&str]) -> u16 {
    signals.iter().fold(0_u16, |count, signal| {
        count.saturating_add(count_ascii_case_insensitive(raw, signal))
    })
}

fn populate_javascript_safety_metrics(metrics: &mut SymbolHealthMetrics, raw: &str) {
    if contains_security_word(raw) {
        metrics.insecure_random = count_ascii_case_insensitive(raw, "math.random(");
    }
    let network_calls = count_javascript_signals(raw, &["fetch(", "axios."]);
    if network_calls > 0
        && !contains_ascii_case_insensitive(raw, "timeout")
        && !contains_ascii_case_insensitive(raw, "signal")
    {
        metrics.http_without_timeout = network_calls;
    }
    let json_parse = count_ascii_case_insensitive(raw, "json.parse(");
    if json_parse > 0 && !contains_ascii_case_insensitive(raw, "try") {
        metrics.unsafe_json_parse = json_parse;
    }
    let env_reads = count_ascii_case_insensitive(raw, "process.env.");
    if env_reads > 0 && !contains_ascii_case_insensitive(raw, "z.") {
        metrics.unvalidated_env = env_reads;
    }
}

fn body_is_empty(raw: &str) -> bool {
    let compact = raw
        .chars()
        .filter(|character| !character.is_whitespace() && !matches!(character, '{' | '}' | ';'))
        .collect::<String>();
    matches!(compact.as_str(), "" | "return" | "returnundefined" | "pass")
}

fn contains_security_word(raw: &str) -> bool {
    [
        "token",
        "secret",
        "password",
        "nonce",
        "salt",
        "csrf",
        "session",
        "auth",
        "apikey",
        "privatekey",
        "accesskey",
    ]
    .iter()
    .any(|needle| contains_ascii_case_insensitive(raw, needle))
}

fn contains_sql_statement_shape(raw: &str) -> bool {
    contains_select_statement_shape(raw)
        || [("insert", "into"), ("update", "set"), ("delete", "from")]
            .iter()
            .any(|(statement, clause)| contains_ordered_words(raw, statement, clause))
}

fn contains_select_statement_shape(raw: &str) -> bool {
    contains_ordered_words(raw, "select", "from")
        && (raw
            .bytes()
            .any(|byte| matches!(byte, b'*' | b',' | b';' | b'='))
            || [
                "where", "join", "group", "order", "limit", "offset", "having",
            ]
            .iter()
            .any(|clause| contains_ordered_words(raw, "from", clause)))
}

fn contains_ordered_words(raw: &str, first: &str, second: &str) -> bool {
    let mut first_seen = false;
    for word in
        raw.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
    {
        if word.is_empty() {
            continue;
        }
        if first_seen && word.eq_ignore_ascii_case(second) {
            return true;
        }
        if word.eq_ignore_ascii_case(first) {
            first_seen = true;
        }
    }
    false
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    count_ascii_case_insensitive(haystack, needle) > 0
}

fn count_ascii_case_insensitive(haystack: &str, needle: &str) -> u16 {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .fold(0_u16, |count, window| {
            count.saturating_add(u16::from(window.eq_ignore_ascii_case(needle.as_bytes())))
        })
}

pub(crate) fn span_for(node: Node<'_>) -> Result<SourceSpan, ExtractError> {
    let start = node.start_position();
    let end = node.end_position();
    let start_line = u32::try_from(start.row)
        .ok()
        .and_then(|line| line.checked_add(1))
        .ok_or(ExtractError::InvalidSpan)?;
    let end_line = u32::try_from(end.row)
        .ok()
        .and_then(|line| line.checked_add(1))
        .ok_or(ExtractError::InvalidSpan)?;
    let start = SourcePosition::new(
        u64::try_from(node.start_byte()).map_err(|_| ExtractError::InvalidSpan)?,
        start_line,
        u32::try_from(start.column).map_err(|_| ExtractError::InvalidSpan)?,
    )
    .map_err(|_| ExtractError::InvalidSpan)?;
    let end = SourcePosition::new(
        u64::try_from(node.end_byte()).map_err(|_| ExtractError::InvalidSpan)?,
        end_line,
        u32::try_from(end.column).map_err(|_| ExtractError::InvalidSpan)?,
    )
    .map_err(|_| ExtractError::InvalidSpan)?;
    SourceSpan::new(start, end).map_err(|_| ExtractError::InvalidSpan)
}

pub(super) fn callable_signature(
    node: Node<'_>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<String>, ExtractError> {
    let Some(parameters) = node
        .child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("parameter"))
    else {
        return Ok(None);
    };
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    ensure_fact_string_length(
        parameters
            .end_byte()
            .saturating_sub(parameters.start_byte()),
    )?;
    let parameter_text = text_for(source, parameters).trim();
    let parenthesized = parameters.kind() == "required_parameter";
    let prefix_bytes = usize::from(parenthesized);
    let mut required = parameter_text
        .len()
        .checked_add(prefix_bytes.saturating_mul(2))
        .ok_or(ExtractError::OutputLimit)?;
    let return_text = if let Some(return_type) = node
        .child_by_field_name("return_type")
        .or_else(|| node.child_by_field_name("result"))
    {
        let length = return_type
            .end_byte()
            .saturating_sub(return_type.start_byte());
        ensure_fact_string_length(length)?;
        (!text_for(source, return_type)
            .trim()
            .trim_start_matches(':')
            .trim()
            .is_empty())
        .then_some(return_type)
    } else {
        None
    };
    if let Some(return_type) = return_text {
        let return_length = text_for(source, return_type)
            .trim()
            .trim_start_matches(':')
            .trim()
            .len();
        required = required
            .checked_add(2)
            .and_then(|length| length.checked_add(return_length))
            .ok_or(ExtractError::OutputLimit)?;
    }
    ensure_fact_string_length(required)?;
    let mut signature = String::new();
    signature
        .try_reserve(required)
        .map_err(|_| ExtractError::OutputLimit)?;
    if parenthesized {
        signature.push('(');
        signature.push_str(parameter_text);
        signature.push(')');
    } else {
        signature.push_str(parameter_text);
    }
    if let Some(return_type) = return_text {
        let return_text = text_for(source, return_type)
            .trim()
            .trim_start_matches(':')
            .trim();
        if !return_text.is_empty() {
            signature.push_str(": ");
            signature.push_str(return_text);
        }
    }
    Ok(Some(signature))
}

pub(super) fn starts_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

#[derive(Default)]
pub(super) struct BodySearchText {
    pub(super) text: String,
    pub(super) truncated: bool,
}

#[derive(Default)]
struct BodySearchAccumulator<'source> {
    prefix: String,
    tail: VecDeque<&'source str>,
    tail_bytes: usize,
    using_tail: bool,
    truncated: bool,
}

pub(super) fn body_search_text(
    root: Node<'_>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<BodySearchText, ExtractError> {
    let mut accumulator = BodySearchAccumulator::default();
    for node in descendants_including_root(root) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let token = if is_search_identifier(node.kind()) {
            text_for(source, node)
        } else if is_search_keyword(node.kind()) {
            node.kind()
        } else {
            continue;
        };
        if token.is_empty() {
            continue;
        }
        accumulator.push(token)?;
    }
    accumulator.finish()
}

impl<'source> BodySearchAccumulator<'source> {
    fn push(&mut self, token: &'source str) -> Result<(), ExtractError> {
        if self.fits_prefix(token) {
            return self.push_prefix(token);
        }
        self.using_tail = true;
        self.push_tail(token)
    }

    fn fits_prefix(&self, token: &str) -> bool {
        let separator = usize::from(!self.prefix.is_empty());
        !self.using_tail
            && self
                .prefix
                .len()
                .checked_add(separator)
                .and_then(|length| length.checked_add(token.len()))
                .is_some_and(|required| required <= BODY_SEARCH_PREFIX_BYTES)
    }

    fn push_prefix(&mut self, token: &str) -> Result<(), ExtractError> {
        let separator = usize::from(!self.prefix.is_empty());
        self.prefix
            .try_reserve(separator.saturating_add(token.len()))
            .map_err(|_| ExtractError::OutputLimit)?;
        if separator != 0 {
            self.prefix.push(' ');
        }
        self.prefix.push_str(token);
        Ok(())
    }

    fn push_tail(&mut self, token: &'source str) -> Result<(), ExtractError> {
        let tail_capacity = MAX_BODY_SEARCH_BYTES
            .saturating_sub(self.prefix.len())
            .saturating_sub(usize::from(!self.prefix.is_empty()));
        if token.len() > tail_capacity {
            self.truncated = true;
            return Ok(());
        }
        while self
            .tail_bytes
            .saturating_add(tail_token_bytes(token, !self.tail.is_empty())?)
            > tail_capacity
        {
            let Some(removed) = self.tail.pop_front() else {
                return Err(ExtractError::OutputLimit);
            };
            self.tail_bytes = self
                .tail_bytes
                .saturating_sub(removed.len())
                .saturating_sub(usize::from(!self.tail.is_empty()));
            self.truncated = true;
        }
        let token_bytes = tail_token_bytes(token, !self.tail.is_empty())?;
        self.tail
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.tail.push_back(token);
        self.tail_bytes = self
            .tail_bytes
            .checked_add(token_bytes)
            .ok_or(ExtractError::OutputLimit)?;
        Ok(())
    }

    fn finish(self) -> Result<BodySearchText, ExtractError> {
        let mut text = self.prefix;
        text.try_reserve(
            usize::from(!text.is_empty() && !self.tail.is_empty()).saturating_add(self.tail_bytes),
        )
        .map_err(|_| ExtractError::OutputLimit)?;
        for token in self.tail {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(token);
        }
        Ok(BodySearchText {
            text,
            truncated: self.truncated,
        })
    }
}

fn tail_token_bytes(token: &str, separated: bool) -> Result<usize, ExtractError> {
    token
        .len()
        .checked_add(usize::from(separated))
        .ok_or(ExtractError::OutputLimit)
}

const SEARCH_IDENTIFIER_KINDS: &[&str] = &[
    "identifier",
    "field_identifier",
    "jsx_identifier",
    "package_identifier",
    "private_property_identifier",
    "property_identifier",
    "shorthand_property_identifier",
    "shorthand_property_identifier_pattern",
    "statement_identifier",
    "type_identifier",
];

fn is_search_identifier(kind: &str) -> bool {
    SEARCH_IDENTIFIER_KINDS.contains(&kind)
}

fn is_search_keyword(kind: &str) -> bool {
    matches!(
        kind,
        "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "continue"
            | "else"
            | "finally"
            | "for"
            | "if"
            | "new"
            | "return"
            | "switch"
            | "throw"
            | "try"
            | "while"
            | "yield"
    )
}

pub(super) fn contains_jsx(
    root: Node<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    for node in descendants_including_root(root) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if matches!(
            node.kind(),
            "jsx_element" | "jsx_opening_element" | "jsx_self_closing_element"
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn is_call_or_construction_target(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let target = match parent.kind() {
        "call_expression" | "call" => parent.child_by_field_name("function"),
        "new_expression" => parent.child_by_field_name("constructor"),
        _ => None,
    };
    target.is_some_and(|target| {
        target.start_byte() == node.start_byte() && target.end_byte() == node.end_byte()
    })
}

/// Whether `node` is the callee of a called Rust turbofish (`f::<T>(..)`,
/// `a::f::<T>(..)`, or `x.f::<T>(..)`), which the call reference names, so it
/// is no separate path or field use.
pub(super) fn is_rust_turbofish_callee(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "generic_function"
            && parent
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == node.id())
            && is_call_or_construction_target(parent)
    })
}

pub(super) fn export_flags(node: Node<'_>) -> (bool, bool) {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "export_statement" {
            return (true, has_child_kind(parent, "default"));
        }
        if is_export_scope_boundary(parent.kind()) {
            return (false, false);
        }
        current = parent.parent();
    }
    (false, false)
}

fn is_export_scope_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function"
            | "abstract_method_signature"
            | "function_expression"
            | "function_declaration"
            | "function_signature"
            | "interface_body"
            | "interface_declaration"
            | "method_definition"
            | "method_signature"
            | "class_declaration"
            | "class_body"
    )
}

pub(super) fn visibility(node: Node<'_>, source: &str) -> Option<Visibility> {
    children(node)
        .find(|child| child.kind() == "accessibility_modifier")
        .and_then(|child| match text_for(source, child).trim() {
            "public" => Some(Visibility::Public),
            "private" => Some(Visibility::Private),
            "protected" => Some(Visibility::Protected),
            "internal" => Some(Visibility::Internal),
            _ => None,
        })
}

pub(super) fn has_child_kind(node: Node<'_>, kind: &str) -> bool {
    children(node).any(|child| child.kind() == kind)
}

pub(super) fn reference_type_node(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "type_identifier" | "identifier" | "member_expression" | "nested_type_identifier" => {
            Some(node)
        }
        "generic_type" => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0)),
        _ => None,
    }
}

pub(crate) fn structural_digest(
    root: Node<'_>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ContentDigest, ExtractError> {
    let mut hasher = blake3::Hasher::new_derive_key("cartograph.v2.structural-digest.2026-07-22");
    for node in descendants_including_root(root) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if node.kind() == "comment" {
            continue;
        }
        hash_field(&mut hasher, node.kind().as_bytes());
        let mut child_count = 0_usize;
        for child in children(node) {
            if cancelled() {
                return Err(ExtractError::Cancelled);
            }
            if child.kind() != "comment" {
                child_count = child_count.saturating_add(1);
            }
        }
        hash_field(&mut hasher, &encoded_length(child_count));
        if child_count == 0 {
            hash_cancellable_field(&mut hasher, text_for(source, node).as_bytes(), cancelled)?;
        }
    }
    Ok(ContentDigest::from_bytes(*hasher.finalize().as_bytes()))
}

pub(crate) fn clone_shape_digest(
    root: Node<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ContentDigest, ExtractError> {
    let mut hasher =
        blake3::Hasher::new_derive_key("cartograph.v2.clone-token-shape-digest.2026-07-24");
    for node in descendants_including_root(root) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if clone_token_class(node.kind()) == CloneTokenClass::Comment
            || children(node).next().is_some()
        {
            continue;
        }
        let normalized = match clone_token_class(node.kind()) {
            CloneTokenClass::Identifier => "I",
            CloneTokenClass::Literal => "L",
            CloneTokenClass::Structural => node.kind(),
            CloneTokenClass::Comment => continue,
        };
        hash_field(&mut hasher, normalized.as_bytes());
    }
    Ok(ContentDigest::from_bytes(*hasher.finalize().as_bytes()))
}

pub(crate) fn clone_token_profile(
    root: Node<'_>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<CloneTokenProfile>, ExtractError> {
    let mut counts = BTreeMap::<u64, u32>::new();
    let mut identifier_counts = BTreeMap::<u64, u32>::new();
    let mut total_tokens = 0_u32;
    let mut identifier_tokens = 0_u32;
    for node in descendants_including_root(root) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let class = clone_token_class(node.kind());
        if class == CloneTokenClass::Comment || children(node).next().is_some() {
            continue;
        }
        total_tokens = match total_tokens.checked_add(1) {
            Some(total) if total <= MAX_CLONE_PROFILE_TOTAL_TOKENS => total,
            _ => return Ok(None),
        };
        let fingerprint = clone_token_fingerprint(
            CloneFingerprintInput {
                class,
                kind: node.kind(),
                text: text_for(source, node),
            },
            cancelled,
        )?;
        if class == CloneTokenClass::Identifier {
            identifier_tokens = identifier_tokens.saturating_add(1);
            let identifier_count = identifier_counts.entry(fingerprint).or_default();
            *identifier_count = identifier_count.saturating_add(1);
        }
        if let Some(count) = counts.get_mut(&fingerprint) {
            *count = count.saturating_add(1);
            continue;
        }
        if counts.len() >= MAX_CLONE_PROFILE_DISTINCT_TOKENS {
            return Ok(None);
        }
        counts.insert(fingerprint, 1);
    }
    if total_tokens == 0 {
        return Ok(None);
    }
    let mut retained = Vec::new();
    retained
        .try_reserve_exact(counts.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    retained.extend(
        counts
            .into_iter()
            .map(|(fingerprint, count)| CloneTokenCount(fingerprint, count)),
    );
    let retained_identifiers = identifier_counts
        .into_iter()
        .map(|(fingerprint, count)| CloneTokenCount(fingerprint, count))
        .collect();
    Ok(Some(CloneTokenProfile::new(CloneTokenProfileInput {
        counts: retained,
        total_tokens,
        identifier_counts: retained_identifiers,
        identifier_tokens,
    })))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CloneTokenClass {
    Comment,
    Identifier,
    Literal,
    Structural,
}

#[derive(Clone, Copy)]
struct CloneFingerprintInput<'text> {
    class: CloneTokenClass,
    kind: &'text str,
    text: &'text str,
}

fn clone_token_class(kind: &str) -> CloneTokenClass {
    if contains_ascii_case_insensitive(kind, "comment") {
        CloneTokenClass::Comment
    } else if contains_ascii_case_insensitive(kind, "identifier") {
        CloneTokenClass::Identifier
    } else if [
        "string", "number", "integer", "float", "char", "rune", "numeric",
    ]
    .into_iter()
    .any(|marker| contains_ascii_case_insensitive(kind, marker))
    {
        CloneTokenClass::Literal
    } else {
        CloneTokenClass::Structural
    }
}

fn clone_token_fingerprint(
    input: CloneFingerprintInput<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<u64, ExtractError> {
    let mut hasher = blake3::Hasher::new_derive_key("cartograph.v2.clone-token.2026-07-24");
    match input.class {
        CloneTokenClass::Identifier => {
            hasher.update(b"identifier\0");
            for chunk in input.text.as_bytes().chunks(HASH_CHUNK_BYTES) {
                if cancelled() {
                    return Err(ExtractError::Cancelled);
                }
                hasher.update(chunk);
            }
        }
        CloneTokenClass::Literal => {
            hasher.update(b"literal");
        }
        CloneTokenClass::Structural => {
            hasher.update(b"structural\0");
            hasher.update(input.kind.as_bytes());
        }
        CloneTokenClass::Comment => return Ok(0),
    }
    let digest = hasher.finalize();
    let mut prefix = [0_u8; std::mem::size_of::<u64>()];
    prefix.copy_from_slice(&digest.as_bytes()[..std::mem::size_of::<u64>()]);
    Ok(u64::from_le_bytes(prefix))
}

fn hash_field(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&encoded_length(field.len()));
    hasher.update(field);
}

fn hash_cancellable_field(
    hasher: &mut blake3::Hasher,
    field: &[u8],
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(), ExtractError> {
    hasher.update(&encoded_length(field.len()));
    for chunk in field.chunks(HASH_CHUNK_BYTES) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        hasher.update(chunk);
    }
    Ok(())
}

fn encoded_length(length: usize) -> [u8; std::mem::size_of::<u64>()] {
    u64::try_from(length).unwrap_or(u64::MAX).to_le_bytes()
}

pub(super) fn jsdoc(
    node: Node<'_>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<String>, ExtractError> {
    let mut anchor = node;
    while anchor
        .parent()
        .is_some_and(|parent| parent.kind() == "export_statement")
        && anchor.prev_named_sibling().is_none()
    {
        if let Some(parent) = anchor.parent() {
            anchor = parent;
        }
    }
    let mut comments = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut closer = anchor;
    let mut sibling = anchor.prev_named_sibling();
    while let Some(comment) = sibling {
        if comments.len() >= MAX_DOC_COMMENT_NODES
            || !is_comment_node(comment.kind())
            || !has_contiguous_comment_gap(
                CommentGapInput {
                    source,
                    left: comment,
                    right: closer,
                },
                cancelled,
            )?
        {
            break;
        }
        if text_for(source, comment).trim_start().starts_with("#!") {
            break;
        }
        let raw_length = comment.end_byte().saturating_sub(comment.start_byte());
        let Some(next_retained) = retained_bytes.checked_add(raw_length) else {
            return Ok(None);
        };
        if ensure_fact_string_length(next_retained).is_err() {
            return Ok(None);
        }
        comments
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        comments.push(comment);
        retained_bytes = next_retained;
        closer = comment;
        sibling = comment.prev_named_sibling();
    }
    if comments.is_empty() {
        return Ok(None);
    }
    comments.reverse();
    normalize_preceding_comments(
        CommentNormalizationInput {
            comments: &comments,
            source,
            retained_bytes,
        },
        cancelled,
    )
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "documentation_comment"
    ) || kind.ends_with("_comment")
}

#[derive(Clone, Copy)]
struct CommentGapInput<'tree, 'source> {
    source: &'source str,
    left: Node<'tree>,
    right: Node<'tree>,
}

#[derive(Clone, Copy)]
struct CommentNormalizationInput<'tree, 'source> {
    comments: &'source [Node<'tree>],
    source: &'source str,
    retained_bytes: usize,
}

fn has_contiguous_comment_gap(
    input: CommentGapInput<'_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    let Some(gap) = input
        .source
        .get(input.left.end_byte()..input.right.start_byte())
    else {
        return Ok(false);
    };
    if ensure_fact_string_length(gap.len()).is_err() {
        return Ok(false);
    }
    let mut line_breaks = 0_usize;
    let mut prior_carriage_return = false;
    for (index, character) in gap.chars().enumerate() {
        if index % TEXT_POLL_STRIDE == 0 && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
        match character {
            '\r' => {
                line_breaks = line_breaks.saturating_add(1);
                prior_carriage_return = true;
            }
            '\n' => {
                if !prior_carriage_return {
                    line_breaks = line_breaks.saturating_add(1);
                }
                prior_carriage_return = false;
            }
            _ => prior_carriage_return = false,
        }
        if line_breaks >= 2 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn normalize_preceding_comments(
    input: CommentNormalizationInput<'_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<String>, ExtractError> {
    let mut normalized = String::new();
    normalized
        .try_reserve(input.retained_bytes)
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut pending_blank_lines = 0_usize;
    {
        let mut appender = CommentAppender {
            normalized: &mut normalized,
            pending_blank_lines: &mut pending_blank_lines,
            cancelled,
        };
        for comment in input.comments {
            if (appender.cancelled)() {
                return Err(ExtractError::Cancelled);
            }
            appender.append(text_for(input.source, *comment))?;
        }
    }
    Ok((!normalized.is_empty()).then_some(normalized))
}

struct CommentAppender<'a> {
    normalized: &'a mut String,
    pending_blank_lines: &'a mut usize,
    cancelled: &'a mut dyn FnMut() -> bool,
}

impl CommentAppender<'_> {
    fn append(&mut self, raw: &str) -> Result<(), ExtractError> {
        let mut lines = raw.lines().peekable();
        let mut first = true;
        while let Some(line) = lines.next() {
            let cleaned = clean_comment_line(line, first, lines.peek().is_none());
            first = false;
            if cleaned.is_empty() || is_decorative_rule_line(cleaned, self.cancelled)? {
                if !self.normalized.is_empty() {
                    *self.pending_blank_lines = self.pending_blank_lines.saturating_add(1);
                }
                continue;
            }
            self.append_clean_line(cleaned)?;
        }
        Ok(())
    }

    fn append_clean_line(&mut self, cleaned: &str) -> Result<(), ExtractError> {
        if self.normalized.is_empty() {
            ensure_fact_string_length(cleaned.len())?;
        } else {
            let separators = self.pending_blank_lines.saturating_add(1);
            let next_length = self
                .normalized
                .len()
                .checked_add(separators)
                .and_then(|length| length.checked_add(cleaned.len()))
                .ok_or(ExtractError::OutputLimit)?;
            ensure_fact_string_length(next_length)?;
            for _ in 0..separators {
                self.normalized.push('\n');
            }
        }
        *self.pending_blank_lines = 0;
        push_cancellable(self.normalized, cleaned, self.cancelled)
    }
}

fn clean_comment_line(mut line: &str, first: bool, last: bool) -> &str {
    line = line.trim();
    if first {
        if let Some(body) = line
            .strip_prefix("/**")
            .or_else(|| line.strip_prefix("/*!"))
            .or_else(|| line.strip_prefix("/*"))
        {
            line = body;
        } else if line.starts_with("//") {
            line = line.trim_start_matches('/').trim_start_matches('!');
        }
    }
    if last {
        line = line.strip_suffix("*/").unwrap_or(line);
    }
    line.trim().strip_prefix('*').unwrap_or(line.trim()).trim()
}

fn is_decorative_rule_line(
    line: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    let mut visible = 0_usize;
    for (index, character) in line.chars().enumerate() {
        if index % TEXT_POLL_STRIDE == 0 && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if character.is_whitespace() {
            continue;
        }
        if !matches!(
            character,
            '-' | '='
                | '*'
                | '_'
                | '~'
                | '#'
                | '/'
                | '+'
                | '<'
                | '>'
                | '·'
                | '•'
                | '─'
                | '━'
                | '┄'
                | '┅'
                | '╌'
                | '╍'
                | '═'
        ) {
            return Ok(false);
        }
        visible = visible.saturating_add(1);
    }
    Ok(visible >= 3)
}

fn push_cancellable(
    output: &mut String,
    value: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(), ExtractError> {
    for (index, character) in value.chars().enumerate() {
        if index % TEXT_POLL_STRIDE == 0 && cancelled() {
            return Err(ExtractError::Cancelled);
        }
        output.push(character);
    }
    Ok(())
}

pub(super) fn unquote(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_circumfix('\'', '\'')
        .or_else(|| trimmed.strip_circumfix('"', '"'))
        .or_else(|| trimmed.strip_circumfix('`', '`'))
        .unwrap_or(trimmed)
}

pub(crate) fn collect_diagnostics(
    root: Node<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<ExtractionDiagnostic>, ExtractError> {
    let mut diagnostics = Vec::new();
    for node in descendants_including_root(root) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if node.is_error() || node.is_missing() {
            diagnostics.push(ExtractionDiagnostic {
                code: DiagnosticCode::SyntaxError,
                span: span_for(node).ok(),
            });
            if diagnostics.len() == MAX_DIAGNOSTICS {
                break;
            }
        }
    }
    Ok(diagnostics)
}

pub(super) fn descendants_including_root(node: Node<'_>) -> Descendants<'_> {
    Descendants::new(node, true)
}

#[derive(Clone, Copy)]
pub(super) struct DescendantQuery<'tree, 'kind> {
    pub(super) node: Node<'tree>,
    pub(super) kind: &'kind str,
    pub(super) depth: usize,
    pub(super) maximum_depth: usize,
}

pub(super) fn descendant_of_kind<'tree>(query: DescendantQuery<'tree, '_>) -> Option<Node<'tree>> {
    if query.depth > query.maximum_depth {
        return None;
    }
    if query.node.kind() == query.kind {
        return Some(query.node);
    }
    named_children(query.node).find_map(|child| {
        descendant_of_kind(DescendantQuery {
            node: child,
            kind: query.kind,
            depth: query.depth.saturating_add(1),
            maximum_depth: query.maximum_depth,
        })
    })
}

pub(super) fn descendants(node: Node<'_>) -> Descendants<'_> {
    Descendants::new(node, false)
}

pub(super) fn children(node: Node<'_>) -> DirectChildren<'_> {
    DirectChildren::new(node, false)
}

pub(super) fn named_children(node: Node<'_>) -> DirectChildren<'_> {
    DirectChildren::new(node, true)
}

pub(super) struct DirectChildren<'tree> {
    cursor: TreeCursor<'tree>,
    started: bool,
    named_only: bool,
    finished: bool,
}

impl<'tree> DirectChildren<'tree> {
    fn new(node: Node<'tree>, named_only: bool) -> Self {
        Self {
            cursor: node.walk(),
            started: false,
            named_only,
            finished: false,
        }
    }
}

impl<'tree> Iterator for DirectChildren<'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            let advanced = if self.started {
                self.cursor.goto_next_sibling()
            } else {
                self.started = true;
                self.cursor.goto_first_child()
            };
            if !advanced {
                self.finished = true;
                return None;
            }
            let node = self.cursor.node();
            if !self.named_only || node.is_named() {
                return Some(node);
            }
        }
        None
    }
}

pub(super) struct Descendants<'tree> {
    cursor: TreeCursor<'tree>,
    depth: usize,
    include_root: bool,
    started: bool,
    finished: bool,
}

impl<'tree> Descendants<'tree> {
    fn new(node: Node<'tree>, include_root: bool) -> Self {
        Self {
            cursor: node.walk(),
            depth: 0,
            include_root,
            started: false,
            finished: false,
        }
    }
}

impl<'tree> Iterator for Descendants<'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.started {
            self.started = true;
            if self.include_root {
                return Some(self.cursor.node());
            }
        }
        if self.cursor.goto_first_child() {
            self.depth = self.depth.saturating_add(1);
            return Some(self.cursor.node());
        }
        loop {
            if self.depth == 0 {
                self.finished = true;
                return None;
            }
            if self.cursor.goto_next_sibling() {
                return Some(self.cursor.node());
            }
            if !self.cursor.goto_parent() {
                self.finished = true;
                return None;
            }
            self.depth = self.depth.saturating_sub(1);
        }
    }
}

fn text_for<'source>(source: &'source str, node: Node<'_>) -> &'source str {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::fmt::Write as _;

    use tree_sitter::Parser;

    use super::{
        clone_shape_digest, clone_token_profile, descendants_including_root, structural_digest,
    };
    use crate::ExtractError;

    const FLAT_DECLARATIONS: usize = 10_000;
    const FLAT_CANCEL_AFTER_POLLS: usize = 64;
    const FLAT_EXPECTED_POLLS: usize = FLAT_CANCEL_AFTER_POLLS + 1;
    const LARGE_TEMPLATE_BYTES: usize = 2 * 1024 * 1024;
    const LARGE_CANCEL_AFTER_POLLS: usize = 20;
    const LARGE_EXPECTED_POLLS: usize = LARGE_CANCEL_AFTER_POLLS + 1;

    #[test]
    fn flat_tree_digest_polls_cancellation_without_collecting_the_tree() {
        let mut source = String::new();
        for index in 0..FLAT_DECLARATIONS {
            assert!(
                writeln!(&mut source, "const value_{index} = {index};").is_ok(),
                "writing to a String is infallible"
            );
        }
        let mut parser = Parser::new();
        let language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        if let Err(error) = parser.set_language(&language) {
            panic!("test grammar setup failed: {error}");
        }
        let Some(tree) = parser.parse(&source, None) else {
            panic!("test parser did not produce a tree");
        };
        let mut polls = 0_usize;
        let result = structural_digest(tree.root_node(), &source, &mut || {
            polls = polls.saturating_add(1);
            polls > FLAT_CANCEL_AFTER_POLLS
        });

        assert_matches!(result, Err(ExtractError::Cancelled));
        assert_eq!(polls, FLAT_EXPECTED_POLLS);
    }

    #[test]
    fn one_large_template_leaf_polls_cancellation_between_hash_chunks() {
        let source = format!("const value = `{}`;", "x".repeat(LARGE_TEMPLATE_BYTES));
        let mut parser = Parser::new();
        let language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        if let Err(error) = parser.set_language(&language) {
            panic!("test grammar setup failed: {error}");
        }
        let Some(tree) = parser.parse(&source, None) else {
            panic!("test parser did not produce a tree");
        };
        let mut polls = 0_usize;
        let result = structural_digest(tree.root_node(), &source, &mut || {
            polls = polls.saturating_add(1);
            polls > LARGE_CANCEL_AFTER_POLLS
        });

        assert_matches!(result, Err(ExtractError::Cancelled));
        assert_eq!(polls, LARGE_EXPECTED_POLLS);
    }

    #[test]
    fn clone_shape_folds_identifiers_literals_comments_but_profile_keeps_names() {
        let left = "function alpha(value) { // note\n const total = value + 42; return total; }";
        let right = "function beta(input) { /* other */ const result = input + 7; return result; }";
        let mut parser = Parser::new();
        let language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser
            .set_language(&language)
            .unwrap_or_else(|error| panic!("test grammar setup failed: {error}"));
        let left_tree = parser
            .parse(left, None)
            .unwrap_or_else(|| panic!("left clone fixture did not parse"));
        let right_tree = parser
            .parse(right, None)
            .unwrap_or_else(|| panic!("right clone fixture did not parse"));
        let left_node = descendants_including_root(left_tree.root_node())
            .find(|node| node.kind() == "function_declaration")
            .unwrap_or_else(|| panic!("left function was not found"));
        let right_node = descendants_including_root(right_tree.root_node())
            .find(|node| node.kind() == "function_declaration")
            .unwrap_or_else(|| panic!("right function was not found"));
        let left_shape = clone_shape_digest(left_node, &mut || false)
            .unwrap_or_else(|error| panic!("left clone shape failed: {error}"));
        let right_shape = clone_shape_digest(right_node, &mut || false)
            .unwrap_or_else(|error| panic!("right clone shape failed: {error}"));
        assert_eq!(left_shape, right_shape);

        let left_profile = clone_token_profile(left_node, left, &mut || false)
            .unwrap_or_else(|error| panic!("left clone profile failed: {error}"))
            .unwrap_or_else(|| panic!("left clone profile was omitted"));
        let right_profile = clone_token_profile(right_node, right, &mut || false)
            .unwrap_or_else(|error| panic!("right clone profile failed: {error}"))
            .unwrap_or_else(|| panic!("right clone profile was omitted"));
        assert_ne!(left_profile.counts(), right_profile.counts());
    }

    #[test]
    fn clone_profile_is_order_independent_while_shape_is_order_sensitive() {
        let left = "function run(a, b) { log(a); if (b) { save(b); } }";
        let right = "function run(a, b) { if (b) { save(b); } log(a); }";
        let mut parser = Parser::new();
        let language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser
            .set_language(&language)
            .unwrap_or_else(|error| panic!("test grammar setup failed: {error}"));
        let left_tree = parser
            .parse(left, None)
            .unwrap_or_else(|| panic!("left partial fixture did not parse"));
        let right_tree = parser
            .parse(right, None)
            .unwrap_or_else(|| panic!("right partial fixture did not parse"));
        let left_node = descendants_including_root(left_tree.root_node())
            .find(|node| node.kind() == "function_declaration")
            .unwrap_or_else(|| panic!("left partial function was not found"));
        let right_node = descendants_including_root(right_tree.root_node())
            .find(|node| node.kind() == "function_declaration")
            .unwrap_or_else(|| panic!("right partial function was not found"));
        assert_ne!(
            clone_shape_digest(left_node, &mut || false),
            clone_shape_digest(right_node, &mut || false)
        );
        assert_eq!(
            clone_token_profile(left_node, left, &mut || false),
            clone_token_profile(right_node, right, &mut || false)
        );
    }
}
