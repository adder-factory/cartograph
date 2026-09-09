//! Lease-owned, bounded indexing supervision for Cartograph v2.

#[cfg(test)]
use cartograph_config as _;
#[cfg(test)]
use cartograph_test_support as _;
#[cfg(test)]
use sqlx_core as _;
#[cfg(test)]
use sqlx_postgres as _;

mod config;
mod native_extraction;
mod native_pipeline;
mod prepare_scope;
mod progress;
mod stage;
mod supervisor;
mod task_scope;

pub use config::{InvalidSupervisorConfig, SupervisorConfig};
pub use native_extraction::{
    NativeExtractionConfigError, NativeExtractionObserver, NativeExtractionObserverError,
    NativeExtractionReport, NativeExtractionRequest, NativeExtractionResult,
    NativeExtractionStageConfig, NativeExtractionStageError, run_native_extraction_stage,
};
pub use native_pipeline::{
    NativeGeneration, NativeGenerationBuild, NativeGenerationStorage, NativeParseCache,
    NativeParseCacheReport, NativePipelineConfig, NativePipelineConfigError,
    NativePipelineDeadlines, NativePipelineError, NativePipelineLimits, NativePipelineParallelism,
    NativePipelineReport, NativeRetainedLimits, NativeSpilledGeneration, ScipOverlayInput,
    build_native_generation, build_native_generation_spilled, build_native_generation_with_scip,
    build_native_generation_with_scip_and_cache, native_parse_cache_contract_digest,
};
pub use prepare_scope::SupervisedPrepareError;
pub use progress::{
    CancellationReason, CancellationSignal, PipelineStage, PipelineStageTiming, ProgressError,
    ProgressReporter, SupervisorContext, SupervisorState, SupervisorStatus,
};
pub use stage::{
    StageCancellation, StageCapacity, StageDeadlinePolicy, StageEnvelope, StageExecution,
    StageFailureKind, StageFold, StageItemBudget, StageItemFailure, StageItemMeta, StageMetrics,
    StageMetricsError, StageMetricsSnapshot, StageOutput, StageRunConfig, StageRunError,
    StageRunner, StageSequence, StageWorkItem, StageWorkload,
};
pub use supervisor::{
    IndexerSupervisor, PipelineFailure, PipelineFailureReason, PipelineFileFailure,
    SupervisorError, SupervisorRequest,
};
pub use task_scope::{ScopedTask, ScopedTaskError};
