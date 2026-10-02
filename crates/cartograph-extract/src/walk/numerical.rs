use cartograph_domain::{ContentDigest, SourceLanguage};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedNumericalSite, budget::numerical_site_budget_bytes,
    identity::numerical_site_id,
};

use super::{ExtractionBuilder, owner_for_node, syntax};

const PROVENANCE: &str = "rust_ast_v1";
const EXACT_CONFIDENCE_PPM: u32 = 1_000_000;
const STRONG_HEURISTIC_CONFIDENCE_PPM: u32 = 900_000;
const BOUNDED_HEURISTIC_CONFIDENCE_PPM: u32 = 650_000;

pub(super) fn enrich(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Rust {
        return Ok(());
    }
    for node in syntax::descendants_including_root(root) {
        builder.context.ensure_active()?;
        match node.kind() {
            "binary_expression" => capture_binary(builder, node)?,
            "type_cast_expression" => capture_narrowing_cast(builder, node)?,
            "call_expression" => capture_call(builder, node)?,
            _ => {}
        }
    }
    Ok(())
}

fn capture_binary(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(operator) = node.child_by_field_name("operator") else {
        return Ok(());
    };
    let operator = builder.context.text(operator);
    if matches!(operator, "<" | "<=" | ">" | ">=")
        && let Some(threshold) = absolute_comparison_threshold(builder, node)
    {
        let tolerance = is_epsilon_like_threshold(builder, threshold);
        return push_site(
            builder,
            node,
            SiteClassification {
                operation: if tolerance {
                    "tolerance_comparison"
                } else {
                    "magnitude_bound_comparison"
                },
                hazard: if tolerance {
                    "absolute_only_tolerance"
                } else {
                    "none_observed"
                },
                precision: classify_precision(builder.context.text(node)),
                confidence_ppm: STRONG_HEURISTIC_CONFIDENCE_PPM,
                unknowns: if tolerance {
                    "relative_scale,input_range"
                } else {
                    "bound_semantics,input_range"
                },
            },
        );
    }
    let operation = match operator {
        "+" => "addition",
        "-" => "subtraction",
        "*" => "multiplication",
        "/" => "division",
        "%" => "remainder",
        _ => return Ok(()),
    };
    let widening = widening_cast_precision(builder, node);
    let (hazard, precision, confidence_ppm, unknowns) = if let Some(precision) = widening {
        (
            "arithmetic_before_widening",
            precision,
            STRONG_HEURISTIC_CONFIDENCE_PPM,
            "operand_precision,overflow_or_rounding",
        )
    } else if operation == "division" {
        (
            "none_observed",
            classify_precision(builder.context.text(node)),
            EXACT_CONFIDENCE_PPM,
            "operand_precision,zero_denominator,finite_result",
        )
    } else {
        (
            "none_observed",
            classify_precision(builder.context.text(node)),
            EXACT_CONFIDENCE_PPM,
            "operand_precision,overflow_or_rounding",
        )
    };
    push_site(
        builder,
        node,
        SiteClassification {
            operation,
            hazard,
            precision,
            confidence_ppm,
            unknowns,
        },
    )
}

fn capture_narrowing_cast(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(target) = node.child_by_field_name("type") else {
        return Ok(());
    };
    let precision = classify_precision(builder.context.text(target));
    if !matches!(precision, "f16" | "bf16") || !feeds_accumulation(node) {
        return Ok(());
    }
    push_site(
        builder,
        node,
        SiteClassification {
            operation: "narrowing_cast",
            hazard: "narrowing_before_accumulation",
            precision,
            confidence_ppm: STRONG_HEURISTIC_CONFIDENCE_PPM,
            unknowns: "accumulator_precision,acceptable_error",
        },
    )
}

fn capture_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(function) = node.child_by_field_name("function") else {
        return Ok(());
    };
    let raw_function = builder.context.text(function);
    let compact = raw_function
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let Ok(compact) = std::str::from_utf8(&compact) else {
        return Ok(());
    };
    let precision = classify_precision(compact);
    let input = NumericalCallInput {
        call: node,
        function,
        precision,
    };
    let classification = if is_method(compact, "sum") {
        reduction_classification(precision)
    } else if let Some(domain) = domain_function(compact) {
        domain_call_classification(builder, input, domain)
    } else if ["min", "max", "clamp"]
        .into_iter()
        .any(|name| is_method(compact, name))
    {
        ordering_call_classification(builder, input)
    } else {
        return Ok(());
    };
    push_site(builder, node, classification)
}

#[derive(Clone, Copy)]
struct NumericalCallInput<'tree, 'text> {
    call: Node<'tree>,
    function: Node<'tree>,
    precision: &'text str,
}

fn reduction_classification(precision: &str) -> SiteClassification<'_> {
    let (hazard, confidence_ppm, unknowns) = if matches!(precision, "f16" | "bf16") {
        (
            "low_precision_reduction",
            STRONG_HEURISTIC_CONFIDENCE_PPM,
            "reduction_order,acceptable_error",
        )
    } else {
        (
            "none_observed",
            EXACT_CONFIDENCE_PPM,
            "accumulator_precision,reduction_order",
        )
    };
    SiteClassification {
        operation: "reduction",
        hazard,
        precision,
        confidence_ppm,
        unknowns,
    }
}

fn domain_call_classification<'text>(
    builder: &ExtractionBuilder<'_, '_>,
    input: NumericalCallInput<'_, 'text>,
    domain: DomainFunction,
) -> SiteClassification<'text> {
    let guarded = domain_call_is_guarded(builder, input.call, domain);
    SiteClassification {
        operation: "domain_sensitive_function",
        hazard: if guarded {
            "none_observed"
        } else {
            "domain_precondition_unknown"
        },
        precision: input.precision,
        confidence_ppm: if guarded {
            STRONG_HEURISTIC_CONFIDENCE_PPM
        } else {
            BOUNDED_HEURISTIC_CONFIDENCE_PPM
        },
        unknowns: if guarded {
            "nonfinite_handling"
        } else {
            "input_domain,nonfinite_handling"
        },
    }
}

fn ordering_call_classification<'text>(
    builder: &ExtractionBuilder<'_, '_>,
    input: NumericalCallInput<'_, 'text>,
) -> SiteClassification<'text> {
    let guarded = defensive_ordering_selection(builder, input.call, input.function);
    SiteClassification {
        operation: "ordering_selection",
        hazard: if guarded {
            "none_observed"
        } else {
            "nan_ordering_unknown"
        },
        precision: input.precision,
        confidence_ppm: if guarded {
            STRONG_HEURISTIC_CONFIDENCE_PPM
        } else {
            BOUNDED_HEURISTIC_CONFIDENCE_PPM
        },
        unknowns: if guarded {
            "input_provenance,nan_policy"
        } else {
            "finite_inputs,nan_policy"
        },
    }
}

#[derive(Clone, Copy)]
struct SiteClassification<'a> {
    operation: &'a str,
    hazard: &'a str,
    precision: &'a str,
    confidence_ppm: u32,
    unknowns: &'a str,
}

fn push_site(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    classification: SiteClassification<'_>,
) -> Result<(), ExtractError> {
    let span = syntax::span_for(node)?;
    let site = ExtractedNumericalSite {
        id: numerical_site_id(
            builder.context.snapshot.file_id(),
            &span,
            [classification.operation, classification.hazard],
        ),
        owner: owner_for_node(builder, node),
        span,
        operation: classification.operation.to_owned(),
        hazard: classification.hazard.to_owned(),
        precision: classification.precision.to_owned(),
        expression_digest: site_expression_digest(builder, node, &classification),
        confidence_ppm: classification.confidence_ppm,
        provenance: PROVENANCE.to_owned(),
        unknowns: classification.unknowns.to_owned(),
    };
    builder.context.budget.reserve_fact(
        numerical_site_budget_bytes(&site),
        [
            site.operation.as_str(),
            site.hazard.as_str(),
            site.precision.as_str(),
            site.provenance.as_str(),
            site.unknowns.as_str(),
        ],
    )?;
    builder.facts.numerical_sites.push(site);
    Ok(())
}

fn site_expression_digest(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    classification: &SiteClassification<'_>,
) -> ContentDigest {
    let mut hasher =
        blake3::Hasher::new_derive_key("cartograph.v2.numerical-expression-site.2026-08-01");
    hasher.update(builder.context.snapshot.content_hash().as_str().as_bytes());
    let start = u64::try_from(node.start_byte()).unwrap_or(u64::MAX);
    let end = u64::try_from(node.end_byte()).unwrap_or(u64::MAX);
    hasher.update(&start.to_be_bytes());
    hasher.update(&end.to_be_bytes());
    hasher.update(classification.operation.as_bytes());
    hasher.update(classification.hazard.as_bytes());
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn widening_cast_precision(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Option<&'static str> {
    let mut parent = node.parent()?;
    if parent.kind() == "parenthesized_expression" {
        parent = parent.parent()?;
    }
    if parent.kind() != "type_cast_expression" {
        return None;
    }
    let target = parent.child_by_field_name("type")?;
    let precision = classify_precision(builder.context.text(target));
    matches!(precision, "f32" | "f64").then_some(precision)
}

fn feeds_accumulation(node: Node<'_>) -> bool {
    let mut current = node.parent();
    for _ in 0..3 {
        let Some(parent) = current else {
            return false;
        };
        if parent.kind() == "compound_assignment_expr"
            && parent
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "+=")
        {
            return true;
        }
        if parent.kind() == "binary_expression"
            && parent
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "+")
        {
            return true;
        }
        if !matches!(
            parent.kind(),
            "parenthesized_expression" | "binary_expression"
        ) {
            return false;
        }
        current = parent.parent();
    }
    false
}

fn absolute_comparison_threshold<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
) -> Option<Node<'tree>> {
    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let left_absolute = contains_abs_call(builder.context.text(left));
    let right_absolute = contains_abs_call(builder.context.text(right));
    match (left_absolute, right_absolute) {
        (true, false) => Some(right),
        (false, true) => Some(left),
        _ => None,
    }
}

fn contains_abs_call(raw: &str) -> bool {
    let compact = raw
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    compact.array_windows().any(|window| window == b".abs(")
}

fn is_epsilon_like_threshold(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    let node = unwrap_parenthesized(node);
    if numeric_literal_value(builder.context.text(node))
        .is_some_and(|value| value.is_sign_positive() && value <= 0.01)
    {
        return true;
    }
    let raw = builder.context.text(node).trim();
    if raw.is_empty()
        || raw.contains(['[', ']', '(', ')', '*', '/', '+'])
        || raw.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return false;
    }
    let terminal = raw
        .rsplit([':', '.'])
        .find(|component| !component.is_empty())
        .unwrap_or(raw)
        .to_ascii_lowercase();
    matches!(
        terminal.as_str(),
        "eps" | "epsilon" | "tol" | "tolerance" | "abs_tol" | "absolute_tolerance"
    ) || terminal.ends_with("_epsilon")
        || terminal.ends_with("_tolerance")
        || terminal.ends_with("_tol")
}

#[derive(Clone, Copy)]
enum DomainFunction {
    UnitInterval,
    NonNegative,
    Positive,
}

fn domain_function(function: &str) -> Option<DomainFunction> {
    if ["acos", "asin"]
        .iter()
        .any(|name| is_method(function, name))
    {
        Some(DomainFunction::UnitInterval)
    } else if is_method(function, "sqrt") {
        Some(DomainFunction::NonNegative)
    } else if ["ln", "log", "log2", "log10"]
        .iter()
        .any(|name| is_method(function, name))
    {
        Some(DomainFunction::Positive)
    } else {
        None
    }
}

fn domain_call_is_guarded(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    function: DomainFunction,
) -> bool {
    let Some(function_node) = call.child_by_field_name("function") else {
        return false;
    };
    let Some(input) = method_receiver(function_node).or_else(|| call_argument(call, 0)) else {
        return false;
    };
    expression_establishes_domain(builder, input, function)
        || immutable_binding_initializer(builder, call, input).is_some_and(|initializer| {
            expression_establishes_domain(builder, initializer, function)
        })
}

fn expression_establishes_domain(
    builder: &ExtractionBuilder<'_, '_>,
    expression: Node<'_>,
    domain: DomainFunction,
) -> bool {
    let expression = unwrap_parenthesized(expression);
    if let Some(value) = numeric_literal_value(builder.context.text(expression)) {
        return domain.accepts(value);
    }
    if expression.kind() != "call_expression" {
        return false;
    }
    let Some(function) = expression.child_by_field_name("function") else {
        return false;
    };
    let raw = compact_text(builder.context.text(function));
    if is_method(&raw, "clamp") {
        let receiver = method_receiver(function);
        let offset = usize::from(receiver.is_none());
        let lower = call_argument(expression, offset)
            .and_then(|value| numeric_literal_value(builder.context.text(value)));
        let upper = call_argument(expression, offset.saturating_add(1))
            .and_then(|value| numeric_literal_value(builder.context.text(value)));
        return lower
            .zip(upper)
            .is_some_and(|(lower, upper)| domain.accepts_clamp(lower, upper));
    }
    if is_method(&raw, "max") {
        return ordering_bound(builder, expression, function)
            .is_some_and(|bound| domain.accepts_floor(bound));
    }
    if matches!(domain, DomainFunction::UnitInterval) && is_method(&raw, "min") {
        let Some(upper) = ordering_bound(builder, expression, function) else {
            return false;
        };
        let Some(receiver) = method_receiver(function) else {
            return false;
        };
        let receiver = unwrap_parenthesized(receiver);
        if receiver.kind() != "call_expression" {
            return false;
        }
        let Some(inner_function) = receiver.child_by_field_name("function") else {
            return false;
        };
        let inner_raw = compact_text(builder.context.text(inner_function));
        return is_method(&inner_raw, "max")
            && ordering_bound(builder, receiver, inner_function)
                .is_some_and(|lower| lower >= -1.0 && upper <= 1.0 && lower <= upper);
    }
    false
}

impl DomainFunction {
    fn accepts(self, value: f64) -> bool {
        value.is_finite()
            && match self {
                Self::UnitInterval => (-1.0..=1.0).contains(&value),
                Self::NonNegative => value >= 0.0,
                Self::Positive => value > 0.0,
            }
    }

    fn accepts_clamp(self, lower: f64, upper: f64) -> bool {
        lower.is_finite()
            && upper.is_finite()
            && lower <= upper
            && match self {
                Self::UnitInterval => lower >= -1.0 && upper <= 1.0,
                Self::NonNegative => lower >= 0.0,
                Self::Positive => lower > 0.0,
            }
    }

    fn accepts_floor(self, floor: f64) -> bool {
        floor.is_finite()
            && match self {
                Self::UnitInterval => false,
                Self::NonNegative => floor >= 0.0,
                Self::Positive => floor > 0.0,
            }
    }
}

fn defensive_ordering_selection(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    function: Node<'_>,
) -> bool {
    if is_method(&compact_text(builder.context.text(function)), "clamp") {
        let offset = usize::from(method_receiver(function).is_none());
        let lower = call_argument(call, offset)
            .and_then(|value| numeric_literal_value(builder.context.text(value)));
        let upper = call_argument(call, offset.saturating_add(1))
            .and_then(|value| numeric_literal_value(builder.context.text(value)));
        return lower.zip(upper).is_some_and(|(lower, upper)| {
            lower.is_finite() && upper.is_finite() && lower <= upper
        });
    }
    ordering_bound(builder, call, function).is_some()
}

fn ordering_bound(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    function: Node<'_>,
) -> Option<f64> {
    let receiver = method_receiver(function);
    let first = call_argument(call, usize::from(receiver.is_none()))?;
    let bound = numeric_literal_value(builder.context.text(first))?;
    if !bound.is_finite() {
        return None;
    }
    Some(bound)
}

fn method_receiver(function: Node<'_>) -> Option<Node<'_>> {
    (function.kind() == "field_expression")
        .then(|| function.child_by_field_name("value"))
        .flatten()
}

fn call_argument(call: Node<'_>, index: usize) -> Option<Node<'_>> {
    let arguments = call.child_by_field_name("arguments")?;
    syntax::named_children(arguments).nth(index)
}

fn immutable_binding_initializer<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    site: Node<'tree>,
    input: Node<'tree>,
) -> Option<Node<'tree>> {
    let input = unwrap_parenthesized(input);
    if input.kind() != "identifier" {
        return None;
    }
    let name = builder.context.text(input);
    let mut statement = site;
    let block = loop {
        let parent = statement.parent()?;
        if parent.kind() == "block" {
            break parent;
        }
        statement = parent;
    };
    let mut selected = None;
    for candidate in syntax::named_children(block).take(256) {
        if candidate.start_byte() >= statement.start_byte() {
            break;
        }
        if candidate.kind() != "let_declaration"
            || builder
                .context
                .text(candidate)
                .trim_start()
                .starts_with("let mut ")
        {
            continue;
        }
        let pattern = candidate.child_by_field_name("pattern")?;
        if pattern.kind() == "identifier" && builder.context.text(pattern) == name {
            selected = candidate.child_by_field_name("value");
        }
    }
    selected
}

fn unwrap_parenthesized(mut node: Node<'_>) -> Node<'_> {
    for _ in 0..8 {
        if !matches!(
            node.kind(),
            "parenthesized_expression" | "reference_expression"
        ) {
            return node;
        }
        let mut children = syntax::named_children(node);
        let Some(child) = children.next() else {
            return node;
        };
        if children.next().is_some() {
            return node;
        }
        node = child;
    }
    node
}

fn compact_text(raw: &str) -> String {
    raw.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn numeric_literal_value(raw: &str) -> Option<f64> {
    let mut raw = raw.trim();
    while let Some(inner) = raw
        .strip_circumfix('(', ')')
        .filter(|inner| !inner.is_empty())
    {
        raw = inner.trim();
    }
    raw = raw
        .strip_suffix("_f32")
        .or_else(|| raw.strip_suffix("_f64"))
        .or_else(|| raw.strip_suffix("f32"))
        .or_else(|| raw.strip_suffix("f64"))
        .unwrap_or(raw);
    if raw.is_empty()
        || raw.bytes().any(|byte| {
            !(byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'e' | b'E' | b'+' | b'-'))
        })
    {
        return None;
    }
    raw.replace('_', "").parse::<f64>().ok()
}

fn is_method(raw: &str, name: &str) -> bool {
    let method = format!(".{name}");
    let associated = format!("::{name}");
    let generic_method = format!(".{name}::<");
    let generic_associated = format!("::{name}::<");
    raw == name
        || raw.ends_with(&method)
        || raw.ends_with(&associated)
        || raw.contains(&generic_method)
        || raw.contains(&generic_associated)
}

fn classify_precision(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("bf16") {
        "bf16"
    } else if lower.contains("f16") {
        "f16"
    } else if lower.contains("f32") {
        "f32"
    } else if lower.contains("f64") {
        "f64"
    } else if [
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    ]
    .iter()
    .any(|candidate| lower.contains(candidate))
    {
        "integer"
    } else {
        "unknown"
    }
}
