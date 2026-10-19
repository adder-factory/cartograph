//! Lease-owned, bounded indexing supervision for Cartograph v2.

#[cfg(test)]
use cartograph_config as _;

mod native_extraction;
mod native_pipeline;
mod progress;
mod rocks_graph;
mod stage;
mod supervisor;
mod task_scope;

pub use native_extraction::{
    NativeExtractionConfigError, NativeExtractionObserver, NativeExtractionObserverError,
    NativeExtractionReport, NativeExtractionRequest, NativeExtractionResult,
    NativeExtractionStageConfig, NativeExtractionStageError, run_native_extraction_stage,
};
pub use native_pipeline::{
    NativeGeneration, NativeGenerationBuild, NativeGenerationStorage, NativeLocalEditRequest,
    NativeObservedFile, NativeObservedManifest, NativeParseCache, NativeParseCacheReport,
    NativePipelineConfig, NativePipelineConfigError, NativePipelineDeadlines, NativePipelineError,
    NativePipelineLimits, NativePipelineParallelism, NativePipelineReport, NativeRetainedLimits,
    ScipOverlayInput, build_native_generation, build_native_generation_with_scip,
    build_native_generation_with_scip_and_cache, native_parse_cache_contract_digest,
    try_native_local_edit,
};
pub use progress::{
    CancellationReason, CancellationSignal, PipelineStage, PipelineStageTiming, ProgressError,
    ProgressReporter, SupervisorState, SupervisorStatus,
};
pub use rocks_graph::{
    NativeBuildProgress, NativeRocksGraphError, build_native_rocks_graph,
    build_native_rocks_graph_observed, build_native_rocks_graph_owned,
};
pub use stage::{
    StageCancellation, StageCapacity, StageDeadlinePolicy, StageEnvelope, StageExecution,
    StageFailureKind, StageFold, StageItemBudget, StageItemFailure, StageItemMeta, StageMetrics,
    StageMetricsError, StageMetricsSnapshot, StageOutput, StageRunConfig, StageRunError,
    StageRunner, StageSequence, StageWorkItem, StageWorkload,
};
pub use supervisor::{PipelineFailure, PipelineFailureReason, PipelineFileFailure};
pub use task_scope::{ScopedTask, ScopedTaskError};
