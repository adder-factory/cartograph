use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use cartograph_db::{
    CartographDatabase, GenerationContents, LeaseFence, NativeGenerationSpill,
    NativeGenerationSpillPolicy, NativeGenerationSpillRequest, PrepareGenerationError,
    PrepareGenerationMutation, PrepareGenerationProgress, ReadyGeneration,
    SpilledGenerationContents, StagedGeneration, StorageError,
};
use thiserror::Error;
use tokio::{
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, timeout_at},
};

/// A supervised prepare/COPY operation failed before returning a ready token.
#[derive(Debug, Error)]
pub enum SupervisedPrepareError {
    /// The one allowed prepare operation already started.
    #[error("Cartograph generation prepare already started for this supervised operation")]
    AlreadyStarted,
    /// The supervisor closed mutation admission before this call.
    #[error("Cartograph generation prepare scope is closed")]
    ScopeClosed,
    /// The retained database task ended without delivering its typed result.
    #[error("Cartograph generation prepare task ended without a result")]
    ResultUnavailable,
    /// PostgreSQL rejected validation, COPY, fencing, or the ready transition.
    #[error(transparent)]
    Database(#[from] PrepareGenerationError),
}

/// Generation contents that one prepare task can drive to a ready generation.
///
/// The memory and spilled paths differ only in which bounded database call they
/// make, so the shared half lives in `spawn_prepare` and each contents type
/// supplies just its own call.
trait PreparableContents: Send + 'static {
    /// Attach the scope's progress channel to these contents.
    #[must_use]
    fn with_prepare_progress(self, progress: PrepareGenerationProgress) -> Self;

    /// Run the bounded prepare that matches this contents type.
    fn prepare(
        self,
        database: CartographDatabase,
        mutation: PrepareGenerationMutation<'_>,
    ) -> impl Future<Output = Result<ReadyGeneration, PrepareGenerationError>> + Send;
}

impl PreparableContents for GenerationContents {
    fn with_prepare_progress(self, progress: PrepareGenerationProgress) -> Self {
        self.with_progress(progress)
    }

    async fn prepare(
        self,
        database: CartographDatabase,
        mutation: PrepareGenerationMutation<'_>,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        database.prepare_generation_bounded(self, mutation).await
    }
}

impl PreparableContents for SpilledGenerationContents {
    fn with_prepare_progress(self, progress: PrepareGenerationProgress) -> Self {
        self.with_progress(progress)
    }

    async fn prepare(
        self,
        database: CartographDatabase,
        mutation: PrepareGenerationMutation<'_>,
    ) -> Result<ReadyGeneration, PrepareGenerationError> {
        database
            .prepare_spilled_generation_bounded(self, mutation)
            .await
    }
}

/// Cloned inputs one prepare task owns for its whole lifetime.
struct PrepareLaunch {
    database: CartographDatabase,
    fence: LeaseFence,
    statement_timeout: Duration,
    progress: PrepareGenerationProgress,
}

#[derive(Clone)]
pub(crate) struct PrepareScope {
    database: CartographDatabase,
    fence: LeaseFence,
    statement_timeout: Duration,
    registry: Arc<Mutex<PrepareRegistry>>,
    progress: PrepareGenerationProgress,
}

struct PrepareRegistry {
    state: PrepareState,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrepareState {
    Open,
    Running,
    Closed,
}

pub(crate) struct PrepareReap {
    pub(crate) all_joined: bool,
}

impl PrepareScope {
    pub(crate) fn new(
        database: CartographDatabase,
        fence: LeaseFence,
        statement_timeout: Duration,
    ) -> Self {
        Self {
            database,
            fence,
            statement_timeout,
            registry: Arc::new(Mutex::new(PrepareRegistry {
                state: PrepareState::Open,
                handle: None,
            })),
            progress: PrepareGenerationProgress::new(),
        }
    }

    pub(crate) async fn prepare(
        &self,
        contents: GenerationContents,
    ) -> Result<ReadyGeneration, SupervisedPrepareError> {
        let receiver = self.start(contents)?;
        match receiver.await {
            Ok(Ok(ready)) => Ok(ready),
            Ok(Err(error)) => Err(SupervisedPrepareError::Database(error)),
            Err(_) => Err(SupervisedPrepareError::ResultUnavailable),
        }
    }

    pub(crate) async fn prepare_spilled(
        &self,
        contents: SpilledGenerationContents,
    ) -> Result<ReadyGeneration, SupervisedPrepareError> {
        let receiver = self.start(contents)?;
        match receiver.await {
            Ok(Ok(ready)) => Ok(ready),
            Ok(Err(error)) => Err(SupervisedPrepareError::Database(error)),
            Err(_) => Err(SupervisedPrepareError::ResultUnavailable),
        }
    }

    pub(crate) fn spill(
        &self,
        generation: &StagedGeneration,
        policy: NativeGenerationSpillPolicy,
    ) -> Result<NativeGenerationSpill, StorageError> {
        NativeGenerationSpill::new(
            self.database.clone(),
            generation,
            NativeGenerationSpillRequest {
                fence: self.fence.clone(),
                policy,
                statement_timeout: self.statement_timeout,
            },
        )
    }

    /// Cloned launch inputs for one prepare task.
    ///
    /// Both prepare paths take the same registry lock, reject the same states,
    /// and clone the same fields; only the contents type and the database call
    /// differ, so the shared half lives here once.
    fn spawn_prepare<Make, Task>(
        &self,
        make: Make,
    ) -> Result<
        oneshot::Receiver<Result<ReadyGeneration, PrepareGenerationError>>,
        SupervisedPrepareError,
    >
    where
        Make: FnOnce(PrepareLaunch) -> Task,
        Task: Future<Output = Result<ReadyGeneration, PrepareGenerationError>> + Send + 'static,
    {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| SupervisedPrepareError::ResultUnavailable)?;
        match registry.state {
            PrepareState::Open => {}
            PrepareState::Running => return Err(SupervisedPrepareError::AlreadyStarted),
            PrepareState::Closed => return Err(SupervisedPrepareError::ScopeClosed),
        }
        let task = make(PrepareLaunch {
            database: self.database.clone(),
            fence: self.fence.clone(),
            statement_timeout: self.statement_timeout,
            progress: self.progress.clone(),
        });
        let (sender, receiver) = oneshot::channel();
        registry.handle = Some(tokio::spawn(async move {
            let _ = sender.send(task.await);
        }));
        registry.state = PrepareState::Running;
        Ok(receiver)
    }

    fn start<Contents>(
        &self,
        contents: Contents,
    ) -> Result<
        oneshot::Receiver<Result<ReadyGeneration, PrepareGenerationError>>,
        SupervisedPrepareError,
    >
    where
        Contents: PreparableContents,
    {
        self.spawn_prepare(move |launch| async move {
            let contents = contents.with_prepare_progress(launch.progress);
            contents
                .prepare(
                    launch.database,
                    PrepareGenerationMutation::new(&launch.fence, launch.statement_timeout),
                )
                .await
        })
    }

    /// Close admission and join the one prepare task.
    ///
    /// A prepare still running here belongs to work the supervisor has
    /// already stopped (cancellation, a deadline, or a failure); a completed
    /// one has already delivered its result. So the in-flight transaction is
    /// asked to roll back at its next cancellation check instead of running
    /// every remaining COPY and derived-relation step before the supervisor
    /// can fail the generation and release its lease.
    pub(crate) async fn close_and_reap(
        &self,
        reap_deadline: Instant,
        hard_deadline: Instant,
    ) -> PrepareReap {
        let Ok(handle) = self.close_and_take() else {
            return PrepareReap { all_joined: false };
        };
        self.progress.cancel();
        let Some(mut handle) = handle else {
            return PrepareReap { all_joined: true };
        };
        if let Ok(joined) = timeout_at(reap_deadline, &mut handle).await {
            return PrepareReap {
                all_joined: joined.is_ok(),
            };
        }
        if let Ok(joined) = timeout_at(hard_deadline, &mut handle).await {
            return PrepareReap {
                all_joined: joined.is_ok(),
            };
        }
        handle.abort();
        let _ = handle.await;
        PrepareReap { all_joined: false }
    }

    fn close_and_take(&self) -> Result<Option<JoinHandle<()>>, ()> {
        let mut registry = self.registry.lock().map_err(|_| ())?;
        let started = registry.state == PrepareState::Running;
        registry.state = PrepareState::Closed;
        if started {
            Ok(registry.handle.take())
        } else {
            Ok(None)
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.registry
            .lock()
            .is_ok_and(|registry| registry.state == PrepareState::Running)
    }

    pub(crate) fn progress_sequence(&self) -> u64 {
        self.progress.sequence()
    }
}
