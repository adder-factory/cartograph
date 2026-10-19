//! Detector thresholds retain the source facts and explanatory evidence.

use super::{corrupt, inventory::Base};
use crate::{insights::StructuralFinding, rocks::RocksError};
use cartograph_domain::Visibility;
use num_traits::ToPrimitive as _;
use serde_json::{Value, json};

struct Rule {
    name: &'static str,
    metric: &'static str,
    field: &'static str,
    thresholds: [f64; 3],
}
const OCCURRENCES: &[Rule] = &[
    rule("magic_number", "magic_numbers", [3.0, 5.0, 8.0]),
    rule("hardcoded_url", "hardcoded_urls", [1.0, 2.0, 3.0]),
    rule(
        "accidental_quadratic",
        "accidental_quadratic",
        [1.0, 1.0, 5.0],
    ),
    rule("empty_catch", "empty_catches", [1.0, 1.0, 3.0]),
    rule("sync_io_in_async", "sync_io_in_async", [1.0, 1.0, 3.0]),
    rule("forof_await", "sequential_await_loops", [1.0, 2.0, 5.0]),
    rule("ts_any_cast", "ts_any_casts", [2.0, 3.0, 5.0]),
    rule("ts_ignore_suppression", "ts_suppressions", [1.0, 1.0, 3.0]),
    rule("agent_debug_log", "debug_logs", [1.0, 1.0, 5.0]),
    rule("incomplete_marker", "incomplete_markers", [1.0, 5.0, 50.0]),
    rule("dynamic_eval", "dynamic_eval", [1.0, 1.0, 1.0]),
    rule("insecure_hash", "insecure_hash", [1.0, 1.0, 3.0]),
    rule("random_for_security", "insecure_random", [1.0, 1.0, 1.0]),
    rule("http_no_timeout", "http_without_timeout", [1.0, 1.0, 3.0]),
    rule(
        "sql_string_concat",
        "sql_string_concatenation",
        [1.0, 1.0, 2.0],
    ),
    rule("unsafe_json_parse", "unsafe_json_parse", [1.0, 1.0, 3.0]),
    rule("env_no_validation", "unvalidated_env", [1.0, 1.0, 3.0]),
];
const fn rule(name: &'static str, field: &'static str, thresholds: [f64; 3]) -> Rule {
    Rule {
        name,
        metric: "occurrences",
        field,
        thresholds,
    }
}

pub(super) fn evaluate(base: &Base<'_>) -> Result<Vec<StructuralFinding>, RocksError> {
    let mut output = Vec::new();
    if callable(base) {
        callable_rules(base, &mut output)?;
        if lines(base) >= 5 {
            for rule in OCCURRENCES {
                append(
                    base,
                    &mut output,
                    rule.name,
                    rule.metric,
                    base.document.health(rule.field),
                    rule.thresholds,
                )?;
            }
        }
    }
    relational_rules(base, &mut output)?;
    Ok(output)
}
fn callable_rules(base: &Base<'_>, rows: &mut Vec<StructuralFinding>) -> Result<(), RocksError> {
    let health = |key| base.document.health(key);
    let code_lines = base
        .document
        .health_or("code_lines", f64::from(lines(base)));
    for (name, metric, value, thresholds) in [
        ("large_method", "lines", code_lines, [100.0, 100.0, 200.0]),
        (
            "complex_method",
            "cyclomatic",
            health("cyclomatic"),
            [15.0, 15.0, 25.0],
        ),
        (
            "nested_complexity",
            "max_nesting",
            health("max_nesting"),
            [5.0, 5.0, 7.0],
        ),
        (
            "complex_conditional",
            "conditional_operands",
            health("max_conditional_operands"),
            [6.0, 6.0, 8.0],
        ),
        (
            "empty_function_body",
            "occurrences",
            health("empty_body"),
            [1.0, 999.0, 999.0],
        ),
    ] {
        append(base, rows, name, metric, value, thresholds)?;
    }
    if !base.symbol.declaration_only && !base.topology.framework {
        append(
            base,
            rows,
            "long_parameter_list",
            "parameters",
            health("parameter_count"),
            [4.0, 5.0, 7.0],
        )?;
    }
    if code_lines >= 50.0 && health("cyclomatic") >= 8.0 {
        let risk = (code_lines / 100.0)
            * (health("cyclomatic") / 10.0)
            * (health("max_nesting") / 3.0).max(1.0)
            * (health("max_conditional_operands") / 4.0).max(1.0);
        append(
            base,
            rows,
            "brain_method",
            "composite_risk",
            risk,
            [5.0, 10.0, 20.0],
        )?;
    }
    if base.recent_prior
        && let Some(prior) = base.prior.filter(|prior| *prior >= 20)
        && f64::from(lines(base)) > f64::from(prior) * 1.5
    {
        append(
            base,
            rows,
            "recently_grew",
            "growth_percent",
            f64::from(lines(base)) * 100.0 / f64::from(prior),
            [150.0, 200.0, 300.0],
        )?;
    }
    if base.document.secrets_actionable {
        append(
            base,
            rows,
            "secrets_handling",
            "confidence_percent",
            health("secrets_score"),
            [50.0, 50.0, 70.0],
        )?;
    }
    if let Some((fraction, percentile)) = base
        .coverage
        .filter(|(fraction, percentile)| *fraction <= 0.5 && *percentile >= 0.9)
    {
        let _ = percentile;
        append(
            base,
            rows,
            "low_coverage",
            "uncovered_percent",
            (1.0 - fraction) * 100.0,
            [50.0, 50.0, 80.0],
        )?;
    }
    Ok(())
}
fn relational_rules(base: &Base<'_>, rows: &mut Vec<StructuralFinding>) -> Result<(), RocksError> {
    let kind = base.symbol.symbol_kind.as_str();
    if kind == "constant" {
        append(
            base,
            rows,
            "stale_doc",
            "disjoint_documented_numbers",
            base.document.health("stale_doc_numbers"),
            [1.0, 999.0, 999.0],
        )?;
    }
    if matches!(kind, "class" | "struct" | "module") && base.topology.complex >= 5 {
        append(
            base,
            rows,
            "god_class",
            "behavioral_methods",
            number(base.topology.behavioral)?,
            [15.0, 40.0, 60.0],
        )?;
    }
    if kind == "method"
        && let Some(envy) = base.envy.filter(|envy| envy.classes.len() <= 2)
        && number(envy.own)? / number(envy.own.checked_add(envy.foreign).ok_or(corrupt())?.max(1))?
            < 1.0 / 3.0
    {
        append(
            base,
            rows,
            "feature_envy",
            "foreign_field_accesses",
            number(envy.foreign)?,
            [6.0, 12.0, 999.0],
        )?;
    }
    if unused_export(base) {
        append(
            base,
            rows,
            "unused_export",
            "external_incoming_edges",
            1.0,
            [1.0, 1.0, 2.0],
        )?;
    }
    if matches!(
        kind,
        "function" | "method" | "component" | "class" | "struct" | "module"
    ) && !base.document.facade_factory
        && !base.topology.framework
    {
        append(
            base,
            rows,
            "high_fan_out",
            "outgoing_dependencies",
            number(base.topology.outgoing)?,
            [25.0, 50.0, 100.0],
        )?;
    }
    append(
        base,
        rows,
        "unresolved_reference_pressure",
        "unresolved_sites",
        number(base.unresolved)?,
        [15.0, 40.0, 100.0],
    )
}
fn unused_export(base: &Base<'_>) -> bool {
    matches!(
        base.symbol.symbol_kind.as_str(),
        "function" | "method" | "class" | "component" | "constant"
    ) && base.symbol.export.exported
        && base.external == 0
        && base.file.language != "python"
        && base.file.normalized_path != "src/index.ts"
        && !base.file.normalized_path.ends_with(".d.ts")
        && !base.symbol.declaration_only
        && !base.topology.function_local
        && !base.topology.framework
        && base.symbol.visibility != Some(Visibility::Public)
        && !base.document.test
}
fn append(
    base: &Base<'_>,
    rows: &mut Vec<StructuralFinding>,
    name: &str,
    metric: &str,
    value: f64,
    thresholds: [f64; 3],
) -> Result<(), RocksError> {
    if !value.is_finite() {
        return Err(corrupt());
    }
    if value >= thresholds[0] {
        let severity = if value >= thresholds[2] {
            "error"
        } else if value >= thresholds[1] {
            "warning"
        } else {
            "info"
        };
        rows.push(finding(
            base,
            name,
            metric,
            value,
            severity,
            detail(base, name)?,
        ));
    }
    Ok(())
}
pub(super) fn finding(
    base: &Base<'_>,
    name: &str,
    metric: &str,
    value: f64,
    severity: &str,
    detail: Option<Value>,
) -> StructuralFinding {
    StructuralFinding {
        symbol_id: base.symbol.symbol_id.to_string(),
        path: base.file.normalized_path.clone(),
        qualified_name: base.symbol.qualified_name.clone(),
        finding: name.to_owned(),
        severity: severity.to_owned(),
        start_line: base.symbol.start_line,
        end_line: base.symbol.end_line,
        metric_name: metric.to_owned(),
        metric: value,
        degree_centrality: base.centrality,
        outgoing_edges: base.topology.outgoing,
        unresolved_references: base.unresolved,
        detail,
    }
}
fn detail(base: &Base<'_>, name: &str) -> Result<Option<Value>, RocksError> {
    let h = |key| base.document.health(key);
    Ok(match name {
        "hardcoded_url" => Some(json!({ "category": if h("hardcoded_url_requests") > 0.0 {
            if h("hardcoded_url_configuration") > 0.0 { "mixed_endpoint_use" } else { "request_destination" }
        } else { "service_configuration" }, "requestDestinations":h("hardcoded_url_requests"),
            "serviceConfiguration":h("hardcoded_url_configuration"), "presentationAbstentions":h("hardcoded_url_presentation_abstentions") })),
        "forof_await" => Some(json!({ "awaitedCall":"owned_await_expression",
            "controlFlowReason":"independent_iterations_without_loop_carried_dependency_or_post_await_exit",
            "dependencyAbstentions":h("serial_await_dependency_loops"), "controlFlowAbstentions":h("serial_await_control_flow_loops"),
            "declaredSerialIntentAbstentions":h("serial_await_intent_loops") })),
        "unresolved_reference_pressure" => Some(
            json!({ "category":"graph_resolution_coverage", "compilerDiagnostic":false,
            "meaning":"references_not_yet_bound_by_cartograph", "verification":"confirm_with_the_language_toolchain_before_editing_source" }),
        ),
        "high_fan_out" => {
            let [calls, modules, types, composition, data] = base.topology.groups;
            let total = calls
                .checked_add(modules)
                .and_then(|n| n.checked_add(types))
                .and_then(|n| n.checked_add(composition))
                .and_then(|n| n.checked_add(data))
                .ok_or(corrupt())?;
            Some(
                json!({ "role":"mixed_orchestration", "dependencyGroups":{ "calls":calls, "modules":modules, "types":types,
                "composition":composition, "dataFlow":data, "other":base.topology.outgoing.saturating_sub(total) },
                "facadeDelegates":h("facade_factory_delegates") }),
            )
        }
        _ => None,
    })
}
fn callable(base: &Base<'_>) -> bool {
    matches!(
        base.symbol.symbol_kind.as_str(),
        "function" | "method" | "component"
    )
}
fn lines(base: &Base<'_>) -> u32 {
    base.symbol.end_line - base.symbol.start_line + 1
}
fn number(value: u64) -> Result<f64, RocksError> {
    value.to_f64().ok_or(corrupt())
}
