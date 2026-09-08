//! Bounded, zero-protobuf-runtime SCIP export and persistent-overlay import.

mod codec;
mod export;
mod import;
mod model;
mod symbol;

pub use codec::{decode_scip_index, encode_scip_index};
pub use export::{
    ScipExport, ScipExportOptions, ScipExportOptionsInput, ScipExportStats, export_snapshot,
};
pub use import::{
    ScipOverlayPlan, ScipOverlayPreparation, ScipOverlayReport, ScipOverlayRequest,
    apply_scip_overlay, apply_scip_overlay_with_cancellation, prepare_scip_overlay,
    scip_overlay_paths,
};
pub use model::{
    CartographScipEdge, SYMBOL_ROLE_DEFINITION, ScipDocument, ScipError, ScipIndex, ScipOccurrence,
    ScipRelationship, ScipRelationshipRoles, ScipSymbolInformation,
};
