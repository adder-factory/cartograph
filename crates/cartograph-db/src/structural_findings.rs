//! Generation-fenced cache for the deterministic structural finding relation.
//!
//! Evaluating every detector over a whole published generation is far too slow
//! for a readiness probe, so the complete relation is computed once per exact
//! input fingerprint and then served from storage. The fingerprint covers every
//! input that can change while one generation stays current: the generation
//! itself, the superseded generation the growth detector compares against,
//! imported coverage, materialised similarity, the calendar day that bounds the
//! growth window, and the detector contract compiled into this binary.

use serde::Serialize;

/// Outcome of one bounded cache reconciliation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StructuralFindingRefresh {
    /// The stored relation already matched the exact input fingerprint.
    Current,
    /// The complete detector relation was recomputed and replaced.
    Recomputed,
    /// The project has no published generation to analyse.
    NoCurrentGeneration,
}
