//! Reuse complete byte observations from the existing project source scan.

use super::{
    NativePipelineError, NativeStageContext, PipelineStage, SourceExclusionEvidence,
    SourceManifest, SourceManifestEntry, StageCapacity, StageEnvelope, StageExecution, StageFold,
    StageItemBudget, StageItemFailure, StageItemMeta, StageOutput, StageRunConfig, StageSequence,
    StageWorkItem, StageWorkload, block_in_place, planned_item_deadline, usize_to_u64,
};

/// Exact file identity obtained from a bounded, content-hashed snapshot.
pub struct NativeObservedFile(SourceManifestEntry);

impl NativeObservedFile {
    /// Retain metadata only; source bytes remain owned by the source reader.
    #[must_use]
    pub fn from_snapshot(snapshot: &cartograph_extract::SourceSnapshot) -> Self {
        Self(SourceManifestEntry {
            path: snapshot.path().clone(),
            language: snapshot.language(),
            file_id: snapshot.file_id().clone(),
            content_hash: snapshot.content_hash().clone(),
            byte_size: snapshot.byte_size(),
        })
    }
}

/// Optional bounded manifest accelerator; exhaustion selects normal discovery.
pub struct NativeObservedManifest {
    files: Vec<NativeObservedFile>,
    retained: u64,
    source_bytes: u64,
    skipped: u64,
    exclusions: SourceExclusionEvidence,
}

impl NativeObservedManifest {
    /// Start with discovery evidence from the exact owning source scan.
    #[must_use]
    pub fn new(exclusions: SourceExclusionEvidence) -> Self {
        Self {
            files: Vec::new(),
            retained: 0,
            source_bytes: 0,
            skipped: 0,
            exclusions,
        }
    }

    /// Admit one observation in discovery order. False requires dropping this
    /// optional accelerator and using the ordinary complete builder.
    pub fn push(&mut self, file: NativeObservedFile) -> bool {
        let Some(retained) = self.retained.checked_add(file.0.modeled_retained_bytes()) else {
            return false;
        };
        let Some(bytes) = self.source_bytes.checked_add(file.0.byte_size) else {
            return false;
        };
        if retained > 16 * 1_048_576
            || self
                .files
                .last()
                .is_some_and(|previous| previous.0.path >= file.0.path)
            || self.files.try_reserve_exact(1).is_err()
        {
            return false;
        }
        self.files.push(file);
        self.retained = retained;
        self.source_bytes = bytes;
        true
    }

    /// Preserve oversized-file accounting without admitting its bytes.
    pub fn record_oversized(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }
}

pub(super) async fn run(
    stages: &NativeStageContext<'_>,
    source: NativeObservedManifest,
) -> Result<(SourceManifest, SourceExclusionEvidence, u64), NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let reservation = source
        .retained
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(4096))
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Read,
        })?;
    let input = StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(
                reservation.max(1),
                source.source_bytes,
                planned_item_deadline(config.deadlines.item_timeout, deadline.deadline()),
            ),
        ),
        source,
    );
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Read, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(
            [input],
            move |item: StageWorkItem<u8, NativeObservedManifest>| async move {
                let cancellation = item.cancellation();
                let (_, _, source) = item.into_parts();
                block_in_place(|| {
                    if source.files.len() > config.limits.discovery_limits.max_files() {
                        return Err(StageItemFailure);
                    }
                    let mut manifest =
                        SourceManifest::new(config.limits.retained.max_manifest_bytes);
                    for file in source.files {
                        if cancellation.is_cancelled()
                            || file.0.byte_size
                                > usize_to_u64(config.limits.source_limits.max_source_bytes())
                        {
                            return Err(StageItemFailure);
                        }
                        manifest.push(file.0)?;
                    }
                    Ok((manifest, source.exclusions, source.skipped))
                })
            },
        ),
        StageFold::new(
            None,
            |result: &mut Option<(SourceManifest, SourceExclusionEvidence, u64)>,
             output: StageOutput<u8, (SourceManifest, SourceExclusionEvidence, u64)>| {
                *result = Some(output.into_parts().1);
                Ok(())
            },
        ),
    );
    stages
        .runner
        .execute(execution)
        .await?
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Read,
        })
}
