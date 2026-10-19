use cartograph_db::CanonicalGenerationFacts;

use super::{
    GraphPolicy, GraphPublicationInput, GraphRevision, NativePipelineError, NativeRocksGraphError,
    PipelineStage, RocksError, StageRunner, StorageRequest,
};
use crate::{
    NativePipelineDeadlines, StageCapacity, StageEnvelope, StageExecution, StageFold,
    StageItemBudget, StageItemFailure, StageItemMeta, StageOutput, StageRunConfig, StageSequence,
    StageWorkItem, StageWorkload,
};

#[cfg(test)]
mod tests;

pub(super) async fn run(
    runner: &StageRunner,
    facts: CanonicalGenerationFacts,
    identity: GraphRevision,
    policy: Policy,
    request: &StorageRequest,
) -> Result<GraphPublicationInput, NativeRocksGraphError> {
    run_with(
        runner,
        facts,
        identity,
        policy,
        request,
        GraphPublicationInput::prepare,
    )
    .await
}

pub(super) struct Policy {
    pub(super) graph: GraphPolicy,
    pub(super) maximum_bytes: u64,
    pub(super) deadlines: NativePipelineDeadlines,
}

async fn run_with<Work>(
    runner: &StageRunner,
    facts: CanonicalGenerationFacts,
    identity: GraphRevision,
    policy: Policy,
    request: &StorageRequest,
    work: Work,
) -> Result<GraphPublicationInput, NativeRocksGraphError>
where
    Work: Fn(
            GraphRevision,
            CanonicalGenerationFacts,
            GraphPolicy,
            u64,
            &StorageRequest,
        ) -> Result<GraphPublicationInput, RocksError>
        + Send
        + Sync
        + 'static,
{
    let (deadlines, item_deadline) = policy.deadlines.publication_policy();
    let work = std::sync::Arc::new(work);
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(policy.maximum_bytes, 0, item_deadline),
        ),
        (facts, identity, request.clone()),
    )];
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Copy, StageCapacity::new(1, 0), deadlines),
        StageWorkload::new(
            inputs,
            move |item: StageWorkItem<
                u8,
                (CanonicalGenerationFacts, GraphRevision, StorageRequest),
            >| {
                let work = std::sync::Arc::clone(&work);
                async move {
                    let cancellation = item.cancellation();
                    let (_, _, (facts, identity, request)) = item.into_parts();
                    let result = tokio::task::block_in_place(|| {
                        let request = cancellation.storage_request(&request)?;
                        work(
                            identity,
                            facts,
                            policy.graph,
                            policy.maximum_bytes,
                            &request,
                        )
                    });
                    Ok::<_, StageItemFailure>(result)
                }
            },
        ),
        StageFold::new(
            None,
            |output: &mut Option<Result<GraphPublicationInput, RocksError>>,
             item: StageOutput<u8, Result<GraphPublicationInput, RocksError>>| {
                *output = Some(item.into_parts().1);
                Ok(())
            },
        ),
    );
    runner
        .execute(execution)
        .await
        .map_err(NativePipelineError::from)?
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Copy,
        })?
        .map_err(NativeRocksGraphError::from)
}
