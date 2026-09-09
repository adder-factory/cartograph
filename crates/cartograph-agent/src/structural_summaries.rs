use std::{collections::BTreeSet, sync::Arc};

use crate::{ProjectCancellation, ProjectError, ProjectRuntime};
use cartograph_db::{
    PendingStructuralSummary, PendingStructuralSummaryQuery, StructuralSummaryEdge,
    StructuralSymbolSummarySaveInput, SummaryCandidatePolicy,
};
use cartograph_domain::{ContentDigest, GenerationId, ProjectId, SymbolId};
use serde::Serialize;

/// Maximum symbols admitted by one structural summary page.
pub const STRUCTURAL_SUMMARY_PAGE_SIZE: u16 = 320;
/// Stable model identity for deterministic structural summaries.
pub const STRUCTURAL_SUMMARY_MODEL: &str = "structural:v2";
const SUMMARY_MAXIMUM_TEXT_BYTES: usize = 200;

/// Typed structural-summary outcome, shared by CLI, MCP and embedded callers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralSummaryReport {
    generation_id: GenerationId,
    candidates: u64,
    generated: u64,
    unmatched: u64,
    preserved_or_source_changed: u64,
    source_changed: bool,
    model: &'static str,
    generation_mode: &'static str,
    patterns: [&'static str; 9],
}

impl StructuralSummaryReport {
    /// Number of source-digest-compatible structural summaries saved.
    #[must_use]
    pub const fn generated(&self) -> u64 {
        self.generated
    }
    /// Whether publication replaced the generation while the sweep was running.
    #[must_use]
    pub const fn source_changed(&self) -> bool {
        self.source_changed
    }
}

#[derive(Default)]
struct StructuralSummarySweepStats {
    candidates: u64,
    generated: u64,
    unmatched: u64,
    preserved_or_source_changed: u64,
    source_changed: bool,
}

struct StructuralSummaryContext<'context> {
    runtime: Arc<ProjectRuntime>,
    project_id: ProjectId,
    expected_generation: GenerationId,
    policy: &'context SummaryCandidatePolicy,
    cancellation: ProjectCancellation,
}

/// Generate structural symbol summaries under a generation and source-digest fence.
///
/// Candidate pages are bounded; cancellation stops work between each page and write.
/// Existing higher-quality summaries are preserved by the database boundary.
/// # Errors
/// Returns a typed error for cancellation, absent generation, invalid evidence,
/// or failed reads/writes. A replaced generation is reported as source changed.
pub async fn run_structural_summary_sweep(
    runtime: Arc<ProjectRuntime>,
    policy: &SummaryCandidatePolicy,
    cancellation: ProjectCancellation,
) -> Result<StructuralSummaryReport, ProjectError> {
    let snapshot = runtime
        .database()
        .project_snapshot_by_root(runtime.root_identity())
        .await
        .map_err(|_| ProjectError::StatusFailed)?
        .ok_or(ProjectError::StatusFailed)?;
    let context = StructuralSummaryContext {
        expected_generation: snapshot
            .current
            .as_ref()
            .map(|current| current.generation_id.clone())
            .ok_or(ProjectError::StatusFailed)?,
        project_id: snapshot.project_id,
        runtime,
        policy,
        cancellation,
    };
    let mut after = None::<SymbolId>;
    let mut stats = StructuralSummarySweepStats::default();
    loop {
        if context.cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let pending = context
            .runtime
            .database()
            .pending_structural_summaries(
                PendingStructuralSummaryQuery::new(
                    &context.project_id,
                    STRUCTURAL_SUMMARY_PAGE_SIZE,
                    context.policy,
                )
                .after_symbol(after.as_ref()),
            )
            .await
            .map_err(|_| ProjectError::EnrichmentReadFailed)?;
        if pending.is_empty() {
            break;
        }
        let last_id = pending
            .last()
            .map(|candidate| candidate.symbol_id().to_owned())
            .ok_or(ProjectError::EnrichmentDataInvalid)?;
        persist_structural_summary_page(&context, pending, &mut stats).await?;
        if stats.source_changed {
            break;
        }
        after = Some(SymbolId::parse(&last_id).map_err(|_| ProjectError::IndexFailed)?);
    }
    finish_structural_summary_sweep(&context, stats).await
}

async fn persist_structural_summary_page(
    context: &StructuralSummaryContext<'_>,
    pending: Vec<PendingStructuralSummary>,
    stats: &mut StructuralSummarySweepStats,
) -> Result<(), ProjectError> {
    for candidate in pending {
        if context.cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        if candidate.generation_id() != context.expected_generation.as_str() {
            stats.source_changed = true;
            break;
        }
        stats.candidates = stats.candidates.saturating_add(1);
        let Some(summary) = structural_summary_for(&candidate) else {
            stats.unmatched = stats.unmatched.saturating_add(1);
            continue;
        };
        let symbol_id = SymbolId::parse(candidate.symbol_id())
            .map_err(|_| ProjectError::EnrichmentDataInvalid)?;
        let source_digest = ContentDigest::parse(candidate.content_hash())
            .map_err(|_| ProjectError::EnrichmentDataInvalid)?;
        match context
            .runtime
            .database()
            .save_structural_symbol_summary(
                StructuralSymbolSummarySaveInput::new(
                    &context.project_id,
                    &symbol_id,
                    &source_digest,
                )
                .with_summary(&summary),
            )
            .await
            .map_err(|_| ProjectError::EnrichmentWriteFailed)?
        {
            Some(_) => stats.generated = stats.generated.saturating_add(1),
            None => {
                stats.preserved_or_source_changed =
                    stats.preserved_or_source_changed.saturating_add(1);
            }
        }
    }
    Ok(())
}

async fn finish_structural_summary_sweep(
    context: &StructuralSummaryContext<'_>,
    mut stats: StructuralSummarySweepStats,
) -> Result<StructuralSummaryReport, ProjectError> {
    let latest = context
        .runtime
        .database()
        .project_snapshot_by_root(context.runtime.root_identity())
        .await
        .map_err(|_| ProjectError::StatusFailed)?
        .and_then(|snapshot| snapshot.current)
        .map(|current| current.generation_id.clone());
    stats.source_changed |= latest.as_ref() != Some(&context.expected_generation);
    Ok(StructuralSummaryReport {
        generation_id: context.expected_generation.clone(),
        candidates: stats.candidates,
        generated: stats.generated,
        unmatched: stats.unmatched,
        preserved_or_source_changed: stats.preserved_or_source_changed,
        source_changed: stats.source_changed,
        model: STRUCTURAL_SUMMARY_MODEL,
        generation_mode: "structural_rule",
        patterns: [
            "route",
            "one_call_forwarder",
            "one_instantiation_factory",
            "single_field_accessor",
            "cross_file_reexport",
            "type_alias",
            "declaration_only",
            "typed_relationships",
            "safe_signature_fallback",
        ],
    })
}

fn structural_summary_for(candidate: &PendingStructuralSummary) -> Option<String> {
    structural_route_summary(candidate)
        .or_else(|| structural_simple_summary(candidate))
        .or_else(|| structural_reexport_summary(candidate))
        .or_else(|| structural_alias_summary(candidate))
        .or_else(|| structural_declaration_summary(candidate))
        .or_else(|| structural_relationship_summary(candidate))
        .or_else(|| Some(structural_signature_summary(candidate)))
}

fn structural_route_summary(candidate: &PendingStructuralSummary) -> Option<String> {
    if candidate.symbol_kind() != "route" {
        return None;
    }
    let route = if candidate.name().to_ascii_lowercase().starts_with("cmd ") {
        format!(
            "CLI command {}",
            candidate.name().trim_start_matches("cmd ")
        )
    } else {
        format!("HTTP {}", candidate.name())
    };
    Some(structural_edges(candidate, "calls").first().map_or_else(
        || cap_structural_summary(&route),
        |handler| cap_structural_summary(&format!("{route} (handler: {})", handler.target_name())),
    ))
}

fn structural_simple_summary(candidate: &PendingStructuralSummary) -> Option<String> {
    let source_lines = candidate
        .end_line()
        .saturating_sub(candidate.start_line())
        .saturating_add(1);
    if source_lines > 4 {
        return None;
    }
    let calls = structural_edges(candidate, "calls");
    if calls.len() == 1 {
        return Some(cap_structural_summary(&format!(
            "Delegates to {}",
            calls[0].target_name()
        )));
    }
    let instantiations = structural_edges(candidate, "instantiates");
    if instantiations.len() == 1 {
        return Some(cap_structural_summary(&format!(
            "Factory for {}",
            instantiations[0].target_name()
        )));
    }
    let field_accesses = structural_edges(candidate, "field_access");
    if !calls.is_empty() || field_accesses.len() != 1 {
        return None;
    }
    Some(cap_structural_summary(&format!(
        "{} {}",
        structural_accessor_verb(candidate),
        field_accesses[0].target_name()
    )))
}

fn structural_accessor_verb(candidate: &PendingStructuralSummary) -> &'static str {
    let name = candidate.name().to_ascii_lowercase();
    let signature = candidate.signature().to_ascii_lowercase();
    if name.starts_with("get_") || name.starts_with("get") || signature.contains(" get ") {
        "Gets"
    } else if name.starts_with("set_") || name.starts_with("set") || signature.contains(" set ") {
        "Sets"
    } else {
        "Accesses"
    }
}

fn structural_reexport_summary(candidate: &PendingStructuralSummary) -> Option<String> {
    if !matches!(
        candidate.symbol_kind(),
        "function" | "type_alias" | "interface" | "class" | "method"
    ) || candidate.end_line().saturating_sub(candidate.start_line()) > 1
    {
        return None;
    }
    structural_edges(candidate, "references")
        .into_iter()
        .find(|edge| edge.target_path() != candidate.path())
        .map(|target| {
            cap_structural_summary(&format!(
                "Re-exports {} from {}",
                target.target_name(),
                target.target_path()
            ))
        })
}

fn structural_alias_summary(candidate: &PendingStructuralSummary) -> Option<String> {
    if candidate.symbol_kind() != "type_alias" || !candidate.edges().is_empty() {
        return None;
    }
    let evidence = if candidate.signature().contains('=') {
        candidate.signature()
    } else {
        candidate.code()
    };
    let (_, right) = evidence.split_once('=')?;
    let right = right.trim().trim_end_matches(';').trim();
    (!right.is_empty()).then(|| cap_structural_summary(&format!("Type alias for {right}")))
}

fn structural_declaration_summary(candidate: &PendingStructuralSummary) -> Option<String> {
    let declaration = candidate.declaration_only()
        || (candidate.start_line() == candidate.end_line()
            && candidate.signature().trim_end().ends_with(';'));
    declaration.then(|| {
        cap_structural_summary(&format!(
            "Declaration-only {}: {}",
            candidate.symbol_kind(),
            candidate.name()
        ))
    })
}

fn structural_relationship_summary(candidate: &PendingStructuralSummary) -> Option<String> {
    let mut relationships = Vec::new();
    for (kind, verb) in [
        ("calls", "calls"),
        ("instantiates", "instantiates"),
        ("extends", "extends"),
        ("implements", "implements"),
        ("overrides", "overrides"),
        ("tests", "tests"),
    ] {
        let edges = structural_edges(candidate, kind);
        if !edges.is_empty() {
            relationships.push(format!("{verb} {}", structural_target_list(&edges, 3)));
        }
    }
    (!relationships.is_empty()).then(|| {
        cap_structural_summary(&format!(
            "{} {} {}",
            structural_kind_label(candidate.symbol_kind()),
            candidate.name(),
            relationships.join("; ")
        ))
    })
}

fn structural_signature_summary(candidate: &PendingStructuralSummary) -> String {
    let signature = candidate.signature().trim();
    let summary = if signature.is_empty() {
        format!(
            "Indexed {} {}",
            candidate.symbol_kind().replace('_', " "),
            candidate.name()
        )
    } else {
        format!(
            "{} {}: {signature}",
            structural_kind_label(candidate.symbol_kind()),
            candidate.name()
        )
    };
    cap_structural_summary(&summary)
}

fn structural_target_list(edges: &[&StructuralSummaryEdge], limit: usize) -> String {
    let shown = edges
        .iter()
        .take(limit)
        .map(|edge| edge.target_name())
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = edges.len().saturating_sub(limit);
    if omitted == 0 {
        shown
    } else {
        format!("{shown} (+{omitted} more)")
    }
}

fn structural_kind_label(kind: &str) -> String {
    let mut label = kind.replace('_', " ");
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    label
}

fn structural_edges<'candidate>(
    candidate: &'candidate PendingStructuralSummary,
    kind: &str,
) -> Vec<&'candidate StructuralSummaryEdge> {
    let mut seen = BTreeSet::new();
    candidate
        .edges()
        .iter()
        .filter(|edge| edge.edge_kind() == kind)
        .filter(|edge| seen.insert(edge.target_symbol_id()))
        .collect()
}

/// Normalize a structural summary to one line and at most 200 UTF-8 bytes.
#[must_use]
pub fn cap_structural_summary(value: &str) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= SUMMARY_MAXIMUM_TEXT_BYTES {
        return one_line;
    }
    let body_limit = SUMMARY_MAXIMUM_TEXT_BYTES.saturating_sub("...".len());
    let body = &one_line[..crate::utf8_boundary(&one_line, body_limit)];
    format!("{}...", body.trim_end())
}
