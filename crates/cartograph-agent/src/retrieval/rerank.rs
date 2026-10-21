//! Optional cross-encoder ordering after generation-fenced channel fusion.

#[cfg(test)]
mod tests;

use super::{
    HybridSearchInput, ProjectCancellation, ProjectRuntime, RERANK_ENDPOINT_TIMEOUT,
    RerankClientState, RerankReport, RetrievalChannels, RetrievalOptions, RetrievalPreference,
    RetrievalStageError, SEMANTIC_QUERY_TIMEOUT, SemanticReadiness, SemanticStorageError,
    fuse_search, rerank_report_without_candidates,
};
use cartograph_db::RerankDocumentsRequest;
use cartograph_domain::{GenerationId, ProjectId};

pub(super) struct MergedRerankRequest<'request> {
    pub(super) runtime: &'request ProjectRuntime,
    pub(super) project_id: &'request ProjectId,
    pub(super) generation: &'request GenerationId,
    pub(super) query: &'request str,
    pub(super) options: RetrievalOptions,
    pub(super) readiness: SemanticReadiness,
    pub(super) preference: RetrievalPreference,
    pub(super) channels: RetrievalChannels,
    pub(super) client: &'request RerankClientState,
    pub(super) cancellation: &'request ProjectCancellation,
}

pub(super) async fn rerank_merged_candidates(
    input: MergedRerankRequest<'_>,
) -> Result<RetrievalChannels, RetrievalStageError> {
    let MergedRerankRequest {
        runtime,
        project_id,
        generation,
        query,
        options,
        readiness,
        preference,
        channels,
        client,
        cancellation,
    } = input;
    let RerankClientState::Ready(client) = client else {
        return Ok(channels.with_rerank_report(rerank_report_without_candidates(client)?));
    };
    // Joining channels must not double the operator's existing model work budget.
    let shortlist_limit = options.candidate_limit;
    let shortlist = fuse_search(
        HybridSearchInput::new(options.mode, readiness, shortlist_limit)
            .map_err(|_| RetrievalStageError::Failed)?
            .with_preference(preference)
            .with_channels(channels.clone()),
    )
    .map_err(|_| RetrievalStageError::Failed)?;
    if shortlist.items().is_empty() {
        return Ok(channels.with_rerank_report(
            RerankReport::skipped_no_candidates(client.model())
                .map_err(|_| RetrievalStageError::Failed)?,
        ));
    }
    let ids = shortlist
        .items()
        .iter()
        .map(|item| item.document().document_id().clone())
        .collect::<Vec<_>>();
    let request = RerankDocumentsRequest::new(
        project_id.clone(),
        generation.clone(),
        ids.clone(),
        SEMANTIC_QUERY_TIMEOUT,
    )
    .map_err(|_| RetrievalStageError::Failed)?;
    let documents = crate::cancellable_native_read(
        cancellation,
        async { Ok(runtime.database().rerank_documents(request).await) },
        || RetrievalStageError::RequestCancelled,
    )
    .await?;
    let documents = match documents {
        Ok(documents)
            if documents.len() == ids.len()
                && documents
                    .iter()
                    .zip(&ids)
                    .all(|(document, id)| document.document_id() == id) =>
        {
            documents
        }
        Err(SemanticStorageError::CurrentGenerationChanged) => {
            return Err(RetrievalStageError::GenerationChanged);
        }
        Ok(_) | Err(_) => return unavailable(channels, client.model()),
    };
    let text = documents
        .iter()
        .map(|document| document.text().to_owned())
        .collect::<Vec<_>>();
    if text.iter().any(|text| text.trim().is_empty()) {
        return Ok(channels.with_rerank_report(
            RerankReport::skipped_no_text(client.model())
                .map_err(|_| RetrievalStageError::Failed)?,
        ));
    }
    let batch = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RetrievalStageError::RequestCancelled),
        result = tokio::time::timeout(RERANK_ENDPOINT_TIMEOUT, client.rerank(query, &text)) => {
            match result {
                Ok(Ok(batch)) => batch,
                Ok(Err(_)) | Err(_) => return unavailable(channels, client.model()),
            }
        }
    };
    if batch.scores().len() != ids.len() {
        return unavailable(channels, client.model());
    }
    let scores = ids
        .into_iter()
        .zip(batch.scores().iter().copied())
        .collect();
    match channels.clone().with_fused_rerank(batch.model(), scores) {
        Ok(reranked) => Ok(reranked),
        Err(_) => unavailable(channels, client.model()),
    }
}

fn unavailable(
    channels: RetrievalChannels,
    model: &str,
) -> Result<RetrievalChannels, RetrievalStageError> {
    Ok(channels.with_rerank_report(
        RerankReport::unavailable(Some(model.to_owned()))
            .map_err(|_| RetrievalStageError::Failed)?,
    ))
}
