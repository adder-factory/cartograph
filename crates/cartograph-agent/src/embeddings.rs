use std::{collections::VecDeque, time::Duration};

use cartograph_db::{
    CartographDatabase, EmbeddingBatchUpsertInput, EmbeddingBatchUpsertRequest,
    EmbeddingModelRegistration, EmbeddingModelRegistrationInput, EmbeddingModelSelector,
    EmbeddingNormalization, EmbeddingPageCursor, EmbeddingUpsertRow, PendingEmbeddingDocument,
    PendingEmbeddingPage, PendingEmbeddingPageInput, PendingEmbeddingPageRequest,
    RegisteredEmbeddingModel, SemanticReadinessReport, SemanticReadinessRequest,
    SemanticStorageError,
};
use cartograph_domain::{GenerationId, ProjectId};
use cartograph_llm::{EmbeddingModelIdentity, EmbeddingSettings, OpenAiEmbeddingClient};
use serde::Serialize;
use tokio::task::JoinSet;

use crate::{ProjectCancellation, ProjectError, ProjectRuntime};

const DEFAULT_EMBEDDING_WORKERS: u16 = 4;
const MAXIMUM_EMBEDDING_WORKERS: u16 = 16;
const MAXIMUM_PENDING_DOCUMENTS: u16 = 128;
const MAXIMUM_PENDING_SOURCE_BYTES: u64 = 16 * 1_024 * 1_024;
const SEMANTIC_STATEMENT_TIMEOUT: Duration = Duration::from_mins(5);
const MODEL_PROVIDER: &str = "openai-compatible";
const DIMENSION_PROBE: &str = "Cartograph embedding dimension probe";

/// Bounded concurrency policy for one resumable current-generation embedding sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddingOptions {
    maximum_workers: u16,
}

impl EmbeddingOptions {
    /// Override endpoint/database batch concurrency in 1..=16.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `workers` is zero or
    /// exceeds the embedding concurrency ceiling.
    pub const fn with_max_workers(mut self, workers: u16) -> Result<Self, ProjectError> {
        if workers == 0 || workers > MAXIMUM_EMBEDDING_WORKERS {
            return Err(ProjectError::InvalidOptions);
        }
        self.maximum_workers = workers;
        Ok(self)
    }

    const fn maximum_workers(self) -> usize {
        self.maximum_workers as usize
    }
}

impl Default for EmbeddingOptions {
    fn default() -> Self {
        Self {
            maximum_workers: DEFAULT_EMBEDDING_WORKERS,
        }
    }
}

/// Explicit endpoint client and bounded execution controls for embedded hosts and live tests.
pub struct EmbeddingClientRequest {
    client: OpenAiEmbeddingClient,
    options: EmbeddingOptions,
    cancellation: ProjectCancellation,
}

impl EmbeddingClientRequest {
    /// Attach a validated client to one bounded, caller-cancellable sweep.
    #[must_use]
    pub const fn new(
        client: OpenAiEmbeddingClient,
        options: EmbeddingOptions,
        cancellation: ProjectCancellation,
    ) -> Self {
        Self {
            client,
            options,
            cancellation,
        }
    }
}

/// Current model identity and exact PostgreSQL readiness evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingStatusReport {
    model: EmbeddingModelIdentity,
    dimension: u16,
    readiness: SemanticReadinessReport,
}

impl EmbeddingStatusReport {
    /// Exact current-generation semantic readiness proof.
    #[must_use]
    pub const fn readiness(&self) -> &SemanticReadinessReport {
        &self.readiness
    }
}

/// Terminal counters for one bounded, resumable embedding sweep.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingSweepReport {
    model: EmbeddingModelIdentity,
    dimension: u16,
    generation_id: GenerationId,
    corpus_documents: u64,
    reused_documents: u64,
    endpoint_documents: u64,
    documents: u64,
    batches: u64,
    written: u64,
    unchanged: u64,
    workers: u16,
    readiness: SemanticReadinessReport,
}

impl EmbeddingSweepReport {
    /// Exact readiness after model registration, vector writes, and HNSW verification.
    #[must_use]
    pub const fn readiness(&self) -> &SemanticReadinessReport {
        &self.readiness
    }

    /// Complete current-generation document population.
    #[must_use]
    pub const fn corpus_documents(&self) -> u64 {
        self.corpus_documents
    }

    /// Matching current-generation vectors present before endpoint work began.
    #[must_use]
    pub const fn reused_documents(&self) -> u64 {
        self.reused_documents
    }

    /// Documents submitted to the embedding endpoint during this sweep.
    #[must_use]
    pub const fn endpoint_documents(&self) -> u64 {
        self.endpoint_documents
    }
}

impl ProjectRuntime {
    /// Probe the configured model and return non-mutating current semantic readiness.
    /// # Errors
    ///
    /// Returns an error when endpoint/model configuration is unavailable, the
    /// dimension probe or current-project lookup fails, PostgreSQL readiness
    /// cannot be read, or the operation is cancelled.
    pub async fn embedding_status(&self) -> Result<EmbeddingStatusReport, ProjectError> {
        self.embedding_status_with_cancellation(ProjectCancellation::new())
            .await
    }

    /// Probe readiness with caller-owned cooperative endpoint/database cancellation.
    /// # Errors
    ///
    /// Returns an error when endpoint/model configuration is unavailable, the
    /// dimension probe or current-project lookup fails, PostgreSQL readiness
    /// cannot be read, or `cancellation` wins.
    pub async fn embedding_status_with_cancellation(
        &self,
        cancellation: ProjectCancellation,
    ) -> Result<EmbeddingStatusReport, ProjectError> {
        let client = configured_client(self.project_root_for_host_operations())?;
        self.embedding_status_with_client(client, cancellation)
            .await
    }

    /// Probe readiness with an explicitly constructed, already validated client.
    /// # Errors
    ///
    /// Returns an error when the model probe is invalid or unavailable, no
    /// current indexed project exists, PostgreSQL readiness fails, or
    /// `cancellation` wins.
    pub async fn embedding_status_with_client(
        &self,
        client: OpenAiEmbeddingClient,
        cancellation: ProjectCancellation,
    ) -> Result<EmbeddingStatusReport, ProjectError> {
        let (model, registration) = resolve_model(&client, &cancellation).await?;
        let project_id = self.current_project_id(&cancellation).await?;
        let request = SemanticReadinessRequest::new(
            project_id,
            registration.selector(),
            SEMANTIC_STATEMENT_TIMEOUT,
        )
        .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        let readiness = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.database().semantic_readiness(
                request,
            ) => result.map_err(|_| ProjectError::EmbeddingOperationFailed)?,
        };
        Ok(EmbeddingStatusReport {
            model,
            dimension: registration.selector().dimension(),
            readiness,
        })
    }

    /// Embed every missing current search document, then prove model-specific HNSW readiness.
    /// # Errors
    ///
    /// Returns an error when configured endpoint/model validation fails, no
    /// current indexed project exists, a bounded embedding or PostgreSQL
    /// operation fails, or the sweep is cancelled.
    pub async fn embed_current(
        &self,
        options: EmbeddingOptions,
    ) -> Result<EmbeddingSweepReport, ProjectError> {
        self.embed_current_with_cancellation(options, ProjectCancellation::new())
            .await
    }

    /// Run a resumable embedding sweep with caller-owned cooperative cancellation.
    /// # Errors
    ///
    /// Returns an error when configured endpoint/model validation fails, no
    /// current indexed project exists, a bounded embedding or PostgreSQL
    /// operation fails, or `cancellation` wins.
    pub async fn embed_current_with_cancellation(
        &self,
        options: EmbeddingOptions,
        cancellation: ProjectCancellation,
    ) -> Result<EmbeddingSweepReport, ProjectError> {
        let client = configured_client(self.project_root_for_host_operations())?;
        self.embed_current_with_client(EmbeddingClientRequest::new(client, options, cancellation))
            .await
    }

    /// Run a sweep with an explicitly constructed, already validated endpoint client.
    /// # Errors
    ///
    /// Returns an error when the client returns an invalid model or vector,
    /// no current indexed project exists, a bounded page/write/HNSW operation
    /// fails, an option-derived size overflows, or cancellation wins.
    pub async fn embed_current_with_client(
        &self,
        input: EmbeddingClientRequest,
    ) -> Result<EmbeddingSweepReport, ProjectError> {
        let sweep = self.start_embedding_sweep(input).await?;
        let (generation_id, progress) = self.sweep_pending_embeddings(&sweep).await?;
        let readiness = self.finish_embedding_sweep(&sweep).await?;
        Ok(EmbeddingSweepReport {
            model: sweep.model_identity,
            dimension: sweep.registered.selector().dimension(),
            generation_id,
            corpus_documents: readiness.documents(),
            reused_documents: sweep.preexisting_embedded_documents,
            endpoint_documents: progress.documents,
            documents: progress.documents,
            batches: progress.batches,
            written: progress.written,
            unchanged: progress.unchanged,
            workers: u16::try_from(sweep.options.maximum_workers())
                .map_err(|_| ProjectError::InvalidOptions)?,
            readiness,
        })
    }

    async fn start_embedding_sweep(
        &self,
        input: EmbeddingClientRequest,
    ) -> Result<ActiveEmbeddingSweep, ProjectError> {
        let EmbeddingClientRequest {
            client,
            options,
            cancellation,
        } = input;
        let (model_identity, registration) = resolve_model(&client, &cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let project_id = self.current_project_id(&cancellation).await?;
        let registered = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.database().register_embedding_model(
                registration,
                SEMANTIC_STATEMENT_TIMEOUT,
            ) => result.map_err(|_| ProjectError::EmbeddingOperationFailed)?,
        };
        let readiness_request = SemanticReadinessRequest::new(
            project_id.clone(),
            registered.selector().clone(),
            SEMANTIC_STATEMENT_TIMEOUT,
        )
        .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        let preexisting_readiness = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.database().semantic_readiness(readiness_request) => {
                result.map_err(|_| ProjectError::EmbeddingOperationFailed)?
            }
        };
        let page_bytes = pending_page_bytes(&client, options)?;
        Ok(ActiveEmbeddingSweep {
            client,
            options,
            cancellation,
            model_identity,
            project_id,
            registered,
            preexisting_embedded_documents: preexisting_readiness.embedded(),
            page_bytes,
        })
    }

    async fn sweep_pending_embeddings(
        &self,
        sweep: &ActiveEmbeddingSweep,
    ) -> Result<(GenerationId, SweepProgress), ProjectError> {
        let mut cursor = None;
        let mut progress = SweepProgress::default();
        let mut generation_id = None;
        loop {
            if sweep.cancellation.is_cancelled() {
                return Err(ProjectError::RequestCancelled);
            }
            let page = self
                .fetch_pending_embedding_page(sweep, cursor.take())
                .await?;
            let (page_generation, documents, next_cursor) = page.into_parts();
            if generation_id
                .as_ref()
                .is_some_and(|current| current != &page_generation)
            {
                return Err(ProjectError::EmbeddingOperationFailed);
            }
            generation_id = Some(page_generation.clone());
            if !documents.is_empty() {
                let batches = partition_documents(
                    documents,
                    sweep.client.maximum_batch(),
                    sweep.client.maximum_input_bytes(),
                )?;
                let page_progress = run_embedding_batches(EmbeddingBatchContext {
                    database: self.database().clone(),
                    client: sweep.client.clone(),
                    project_id: sweep.project_id.clone(),
                    generation_id: page_generation,
                    selector: sweep.registered.selector().clone(),
                    cancellation: sweep.cancellation.clone(),
                    maximum_workers: sweep.options.maximum_workers(),
                    batches,
                })
                .await?;
                progress.merge(page_progress)?;
            }
            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        generation_id
            .map(|generation_id| (generation_id, progress))
            .ok_or(ProjectError::EmbeddingOperationFailed)
    }

    async fn fetch_pending_embedding_page(
        &self,
        sweep: &ActiveEmbeddingSweep,
        cursor: Option<EmbeddingPageCursor>,
    ) -> Result<PendingEmbeddingPage, ProjectError> {
        let mut request = PendingEmbeddingPageRequest::new(PendingEmbeddingPageInput {
            project_id: sweep.project_id.clone(),
            model: sweep.registered.selector().clone(),
            maximum_documents: MAXIMUM_PENDING_DOCUMENTS,
            maximum_source_bytes: sweep.page_bytes,
            statement_timeout: SEMANTIC_STATEMENT_TIMEOUT,
        })
        .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        if let Some(cursor) = cursor {
            request = request
                .with_cursor(cursor)
                .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        }
        tokio::select! {
            biased;
            () = sweep.cancellation.cancelled() => Err(ProjectError::RequestCancelled),
            result = self.database().pending_current_embedding_documents(request) => {
                result.map_err(|_| ProjectError::EmbeddingOperationFailed)
            }
        }
    }

    async fn finish_embedding_sweep(
        &self,
        sweep: &ActiveEmbeddingSweep,
    ) -> Result<SemanticReadinessReport, ProjectError> {
        tokio::select! {
            biased;
            () = sweep.cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.database().ensure_embedding_model_hnsw(
                sweep.registered.selector(),
                SEMANTIC_STATEMENT_TIMEOUT,
            ) => result.map_err(|error| match error {
                SemanticStorageError::HnswCreateSharedMemoryUnavailable => {
                    ProjectError::HnswCreateSharedMemoryUnavailable
                }
                _ => ProjectError::EmbeddingOperationFailed,
            })?,
        };
        let request = SemanticReadinessRequest::new(
            sweep.project_id.clone(),
            sweep.registered.selector().clone(),
            SEMANTIC_STATEMENT_TIMEOUT,
        )
        .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        tokio::select! {
            biased;
            () = sweep.cancellation.cancelled() => Err(ProjectError::RequestCancelled),
            result = self.database().semantic_readiness(request) => {
                result.map_err(|_| ProjectError::EmbeddingOperationFailed)
            }
        }
    }

    async fn current_project_id(
        &self,
        cancellation: &ProjectCancellation,
    ) -> Result<ProjectId, ProjectError> {
        let snapshot = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.database().project_snapshot_by_root(self.root_identity()) => {
                result.map_err(|_| ProjectError::StatusFailed)?
            }
        };
        snapshot
            .filter(|snapshot| snapshot.current.is_some())
            .map(|snapshot| snapshot.project_id)
            .ok_or(ProjectError::EmbeddingOperationFailed)
    }
}

struct ActiveEmbeddingSweep {
    client: OpenAiEmbeddingClient,
    options: EmbeddingOptions,
    cancellation: ProjectCancellation,
    model_identity: EmbeddingModelIdentity,
    project_id: ProjectId,
    registered: RegisteredEmbeddingModel,
    preexisting_embedded_documents: u64,
    page_bytes: u64,
}

fn configured_client(
    project_root: &std::path::Path,
) -> Result<OpenAiEmbeddingClient, ProjectError> {
    let settings = EmbeddingSettings::try_from_project(project_root)
        .map_err(|_| ProjectError::EmbeddingConfigurationUnavailable)?
        .ok_or(ProjectError::EmbeddingConfigurationUnavailable)?;
    OpenAiEmbeddingClient::new(settings)
        .map_err(|_| ProjectError::EmbeddingConfigurationUnavailable)
}

async fn resolve_model(
    client: &OpenAiEmbeddingClient,
    cancellation: &ProjectCancellation,
) -> Result<(EmbeddingModelIdentity, EmbeddingModelRegistration), ProjectError> {
    let input = [DIMENSION_PROBE.to_owned()];
    let probe = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
        result = client.embed(&input) => {
            result.map_err(|_| ProjectError::EmbeddingOperationFailed)?
        }
    };
    let dimension =
        u16::try_from(probe.dimension()).map_err(|_| ProjectError::EmbeddingOperationFailed)?;
    let identity = client.identity().clone();
    let registration = EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
        model_id: identity.model_id().clone(),
        fingerprint: identity.fingerprint().clone(),
        provider: MODEL_PROVIDER.to_owned(),
        model_name: identity.model().to_owned(),
        dimension,
        normalization: EmbeddingNormalization::None,
    })
    .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
    Ok((identity, registration))
}

fn pending_page_bytes(
    client: &OpenAiEmbeddingClient,
    options: EmbeddingOptions,
) -> Result<u64, ProjectError> {
    let bytes = client
        .maximum_input_bytes()
        .checked_mul(options.maximum_workers())
        .ok_or(ProjectError::InvalidOptions)?;
    u64::try_from(bytes)
        .map(|bytes| bytes.min(MAXIMUM_PENDING_SOURCE_BYTES))
        .map_err(|_| ProjectError::InvalidOptions)
}

fn partition_documents(
    documents: Vec<PendingEmbeddingDocument>,
    maximum_documents: usize,
    maximum_bytes: usize,
) -> Result<VecDeque<Vec<PendingEmbeddingDocument>>, ProjectError> {
    let mut batches = VecDeque::new();
    let mut batch = Vec::new();
    let mut batch_bytes = 0_usize;
    for document in documents {
        let bytes = document.text().len();
        if bytes == 0 || bytes > maximum_bytes {
            return Err(ProjectError::EmbeddingOperationFailed);
        }
        let next_bytes = batch_bytes
            .checked_add(bytes)
            .ok_or(ProjectError::EmbeddingOperationFailed)?;
        if !batch.is_empty() && (batch.len() >= maximum_documents || next_bytes > maximum_bytes) {
            batches.push_back(std::mem::take(&mut batch));
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes
            .checked_add(bytes)
            .ok_or(ProjectError::EmbeddingOperationFailed)?;
        batch.push(document);
    }
    if !batch.is_empty() {
        batches.push_back(batch);
    }
    Ok(batches)
}

struct EmbeddingBatchContext {
    database: CartographDatabase,
    client: OpenAiEmbeddingClient,
    project_id: ProjectId,
    generation_id: GenerationId,
    selector: EmbeddingModelSelector,
    cancellation: ProjectCancellation,
    maximum_workers: usize,
    batches: VecDeque<Vec<PendingEmbeddingDocument>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SweepProgress {
    documents: u64,
    batches: u64,
    written: u64,
    unchanged: u64,
}

impl SweepProgress {
    fn merge(&mut self, other: Self) -> Result<(), ProjectError> {
        self.documents = self
            .documents
            .checked_add(other.documents)
            .ok_or(ProjectError::EmbeddingOperationFailed)?;
        self.batches = self
            .batches
            .checked_add(other.batches)
            .ok_or(ProjectError::EmbeddingOperationFailed)?;
        self.written = self
            .written
            .checked_add(other.written)
            .ok_or(ProjectError::EmbeddingOperationFailed)?;
        self.unchanged = self
            .unchanged
            .checked_add(other.unchanged)
            .ok_or(ProjectError::EmbeddingOperationFailed)?;
        Ok(())
    }
}

async fn run_embedding_batches(
    mut context: EmbeddingBatchContext,
) -> Result<SweepProgress, ProjectError> {
    let mut tasks = JoinSet::new();
    for _ in 0..context.maximum_workers {
        let Some(batch) = context.batches.pop_front() else {
            break;
        };
        spawn_batch(&mut tasks, &context, batch);
    }
    let mut progress = SweepProgress::default();
    while let Some(result) = tasks.join_next().await {
        let batch_progress = match result {
            Ok(Ok(progress)) => progress,
            Ok(Err(error)) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(error);
            }
            Err(_) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(ProjectError::EmbeddingOperationFailed);
            }
        };
        progress.merge(batch_progress)?;
        if let Some(batch) = context.batches.pop_front() {
            spawn_batch(&mut tasks, &context, batch);
        }
    }
    Ok(progress)
}

fn spawn_batch(
    tasks: &mut JoinSet<Result<SweepProgress, ProjectError>>,
    context: &EmbeddingBatchContext,
    documents: Vec<PendingEmbeddingDocument>,
) {
    let task = EmbeddingBatchTask {
        database: context.database.clone(),
        client: context.client.clone(),
        project_id: context.project_id.clone(),
        generation_id: context.generation_id.clone(),
        selector: context.selector.clone(),
        cancellation: context.cancellation.clone(),
        documents,
    };
    tasks.spawn(task.run());
}

struct EmbeddingBatchTask {
    database: CartographDatabase,
    client: OpenAiEmbeddingClient,
    project_id: ProjectId,
    generation_id: GenerationId,
    selector: EmbeddingModelSelector,
    cancellation: ProjectCancellation,
    documents: Vec<PendingEmbeddingDocument>,
}

impl EmbeddingBatchTask {
    async fn run(self) -> Result<SweepProgress, ProjectError> {
        let inputs = self
            .documents
            .iter()
            .map(|document| document.text().to_owned())
            .collect::<Vec<_>>();
        let embeddings = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.client.embed_background(&inputs) => {
                result.map_err(|_| ProjectError::EmbeddingOperationFailed)?
            }
        };
        let mut rows = Vec::new();
        rows.try_reserve(self.documents.len())
            .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        for (document, vector) in self.documents.iter().zip(embeddings.vectors()) {
            rows.push(
                EmbeddingUpsertRow::new(
                    document.document_id().clone(),
                    document.source_digest().clone(),
                    vector.values().to_vec(),
                )
                .map_err(|_| ProjectError::EmbeddingOperationFailed)?,
            );
        }
        let request = EmbeddingBatchUpsertRequest::new(EmbeddingBatchUpsertInput {
            project_id: self.project_id,
            generation_id: self.generation_id,
            model: self.selector,
            rows,
            statement_timeout: SEMANTIC_STATEMENT_TIMEOUT,
        })
        .map_err(|_| ProjectError::EmbeddingOperationFailed)?;
        let report = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = self.database.upsert_current_document_embeddings(request) => {
                result.map_err(|_| ProjectError::EmbeddingOperationFailed)?
            }
        };
        Ok(SweepProgress {
            documents: u64::from(report.requested()),
            batches: 1,
            written: u64::from(report.written()),
            unchanged: u64::from(report.unchanged()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_ENDPOINT: &str = "http://localhost:8080";

    #[test]
    fn embedding_options_reject_zero_and_unbounded_workers() {
        assert_eq!(
            EmbeddingOptions::default().with_max_workers(0),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            EmbeddingOptions::default().with_max_workers(MAXIMUM_EMBEDDING_WORKERS + 1),
            Err(ProjectError::InvalidOptions)
        );
        assert!(
            EmbeddingOptions::default()
                .with_max_workers(MAXIMUM_EMBEDDING_WORKERS)
                .is_ok()
        );
    }

    #[test]
    fn page_bytes_scale_with_workers_and_stay_bounded() {
        let settings = EmbeddingSettings::new(FIXTURE_ENDPOINT, "fixture", None)
            .unwrap_or_else(|error| panic!("fixture settings failed: {error}"));
        let client = OpenAiEmbeddingClient::new(settings)
            .unwrap_or_else(|error| panic!("fixture client failed: {error}"));
        let serial = pending_page_bytes(
            &client,
            EmbeddingOptions::default()
                .with_max_workers(1)
                .unwrap_or_else(|error| panic!("serial options failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("serial page bytes failed: {error}"));
        let parallel = pending_page_bytes(
            &client,
            EmbeddingOptions::default()
                .with_max_workers(MAXIMUM_EMBEDDING_WORKERS)
                .unwrap_or_else(|error| panic!("parallel options failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("parallel page bytes failed: {error}"));
        assert_eq!(serial, 2 * 1_024 * 1_024);
        assert_eq!(parallel, MAXIMUM_PENDING_SOURCE_BYTES);
    }
}
