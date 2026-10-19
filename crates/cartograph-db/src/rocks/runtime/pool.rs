use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use super::{Cancellation, Result, RocksError, StorageRequest, WorkerPolicy};
use crate::rocks::operation;

type Job = Box<dyn FnOnce(Result<()>) + Send + 'static>;

pub(super) struct StoragePool {
    queue: Arc<Queue>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    writers: Arc<Semaphore>,
    maximum_bytes: u32,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<QueueState>,
    ready: Condvar,
    closing: Cancellation,
}

#[derive(Default)]
struct QueueState {
    next_id: u64,
    jobs: VecDeque<(u64, Job)>,
    finalizer: Option<Job>,
}

struct Admission {
    _slot: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
    _writer: Option<OwnedSemaphorePermit>,
}

struct Reply<T> {
    result: Result<T>,
    _admission: Admission,
}

/// Only destroyed input allocations may be returned from the original permit.
pub(super) struct ByteReclamation<'a> {
    permit: &'a mut OwnedSemaphorePermit,
    remaining: u32,
}

impl ByteReclamation<'_> {
    pub(super) fn release(&mut self, bytes: usize) -> Result<()> {
        let bytes = u32::try_from(bytes).map_err(|_| operation("worker_reclamation"))?;
        let remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| operation("worker_reclamation"))?;
        let released = self
            .permit
            .split(usize::try_from(bytes).map_err(|_| operation("worker_reclamation"))?)
            .ok_or_else(|| operation("worker_reclamation"))?;
        drop(released);
        self.remaining = remaining;
        Ok(())
    }
}

struct CancelOnDrop {
    cancellation: Cancellation,
    queue: Arc<Queue>,
    job_id: Option<u64>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(id) = self.job_id {
            cancel_queued(&self.queue, id, RocksError::Cancelled);
        }
    }
}

impl StoragePool {
    pub(super) fn new(
        policy: WorkerPolicy,
        finalize: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        if !(2..=16).contains(&policy.threads)
            || !(u16::from(policy.threads)..=1_024).contains(&policy.pending_jobs)
            || !(1_048_576..=512 * 1_048_576).contains(&policy.pending_bytes)
        {
            return Err(RocksError::Limit {
                resource: "worker_policy",
            });
        }
        let pool = Self {
            queue: Arc::new(Queue::default()),
            workers: Mutex::new(Vec::new()),
            slots: Arc::new(Semaphore::new(usize::from(policy.pending_jobs))),
            bytes: Arc::new(Semaphore::new(
                usize::try_from(policy.pending_bytes).map_err(|_| RocksError::Limit {
                    resource: "worker_bytes",
                })?,
            )),
            writers: Arc::new(Semaphore::new(usize::from(policy.threads / 2))),
            maximum_bytes: policy.pending_bytes,
        };
        pool.queue
            .jobs
            .lock()
            .map_err(|_| operation("worker_queue"))?
            .finalizer = Some(Box::new(move |_| finalize()));
        for index in 0..policy.threads {
            let queue = Arc::clone(&pool.queue);
            let worker = thread::Builder::new()
                .name(format!("cartograph-storage-{index}"))
                .spawn(move || work_loop(&queue))
                .map_err(|_| operation("worker_start"))?;
            pool.workers
                .lock()
                .map_err(|_| operation("worker_lock"))?
                .push(worker);
        }
        Ok(pool)
    }

    pub(super) async fn run<T, F>(
        &self,
        request: &StorageRequest,
        reservation: u32,
        mutation: bool,
        operation: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(StorageRequest) -> Result<T> + Send + 'static,
    {
        self.run_reclaiming(request, reservation, mutation, 0, move |control, _| {
            operation(control)
        })
        .await
    }

    pub(super) async fn run_reclaiming<T, F>(
        &self,
        request: &StorageRequest,
        reservation: u32,
        mutation: bool,
        maximum_reclamation: u32,
        operation: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(StorageRequest, &mut ByteReclamation<'_>) -> Result<T> + Send + 'static,
    {
        if maximum_reclamation > reservation {
            return Err(crate::rocks::operation("worker_reclamation"));
        }
        let control = request.child(self.queue.closing.clone());
        let mut cancel_on_drop = CancelOnDrop {
            cancellation: control.cancellation.clone(),
            queue: Arc::clone(&self.queue),
            job_id: None,
        };
        control.check()?;
        let mut admission = self.admit(&control, reservation, mutation).await?;
        let (send, mut receive) = oneshot::channel();
        let work_control = control.clone();
        let job = Box::new(move |admitted: Result<()>| {
            let result = admitted.and_then(|()| work_control.check()).and_then(|()| {
                catch_unwind(AssertUnwindSafe(|| {
                    operation(
                        work_control,
                        &mut ByteReclamation {
                            permit: &mut admission.bytes,
                            remaining: maximum_reclamation,
                        },
                    )
                }))
                .map_err(|_| crate::rocks::operation("worker_panicked"))?
            });
            // Reply retains byte admission until the result is consumed or its
            // receiver is dropped, including when the caller stops polling.
            let _sent = send.send(Reply {
                result,
                _admission: admission,
            });
        });
        let id = self.enqueue(job)?;
        cancel_on_drop.job_id = Some(id);
        let reply = tokio::select! {
            reply = &mut receive => reply,
            error = control.stopped() => {
                // Native writes cannot be aborted safely. The owning pool
                // continues to track this job until its real result is known.
                control.cancel();
                cancel_queued(&self.queue, id, error);
                receive.await
            }
        }
        .map_err(|_| crate::rocks::operation("worker_result"))?;
        reply.result
    }

    async fn admit(
        &self,
        request: &StorageRequest,
        reservation: u32,
        mutation: bool,
    ) -> Result<Admission> {
        if reservation == 0 || reservation > self.maximum_bytes {
            return Err(RocksError::Limit {
                resource: "worker_bytes",
            });
        }
        let permits = async {
            // Acquire mutation admission before queue admission, so queued
            // writers cannot occupy every slot and prevent readers entering.
            let writer = if mutation {
                Some(
                    Arc::clone(&self.writers)
                        .acquire_owned()
                        .await
                        .map_err(|_| RocksError::Closed)?,
                )
            } else {
                None
            };
            let slot = Arc::clone(&self.slots)
                .acquire_owned()
                .await
                .map_err(|_| RocksError::Closed)?;
            let bytes = Arc::clone(&self.bytes)
                .acquire_many_owned(reservation)
                .await
                .map_err(|_| RocksError::Closed)?;
            Ok(Admission {
                _slot: slot,
                bytes,
                _writer: writer,
            })
        };
        tokio::select! {
            admitted = permits => admitted,
            error = request.stopped() => Err(error),
        }
    }

    fn enqueue(&self, job: Job) -> Result<u64> {
        let mut state = self
            .queue
            .jobs
            .lock()
            .map_err(|_| operation("worker_queue"))?;
        if self.queue.closing.is_cancelled() {
            return Err(RocksError::Closed);
        }
        let id = state.next_id;
        state.next_id = id.checked_add(1).ok_or(RocksError::Limit {
            resource: "worker_sequence",
        })?;
        state.jobs.push_back((id, job));
        self.queue.ready.notify_one();
        Ok(id)
    }

    pub(super) fn close(&self) {
        // Use the queue mutex for the condition-variable predicate, avoiding a
        // missed shutdown wake between a worker's empty check and its wait.
        if let Ok(_jobs) = self.queue.jobs.lock() {
            self.queue.closing.cancel();
            self.queue.ready.notify_all();
        }
        self.writers.close();
        self.slots.close();
        self.bytes.close();
    }

    pub(super) async fn drain(&self, request: &StorageRequest) -> Result<()> {
        loop {
            if self.reap_finished() {
                return Ok(());
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(2)) => {},
                error = request.stopped() => return Err(error),
            }
        }
    }

    fn reap_finished(&self) -> bool {
        let Ok(mut workers) = self.workers.lock() else {
            return false;
        };
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _joined = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        workers.is_empty()
    }

    pub(super) fn join(&self) {
        if let Ok(mut workers) = self.workers.lock() {
            for worker in workers.drain(..) {
                let _joined = worker.join();
            }
        }
    }
}

impl Drop for StoragePool {
    fn drop(&mut self) {
        self.close();
        self.join();
    }
}

fn work_loop(queue: &Queue) {
    loop {
        let Some(job) = next_job(queue) else {
            return;
        };
        let _finished = catch_unwind(AssertUnwindSafe(|| job(Ok(()))));
    }
}

fn next_job(queue: &Queue) -> Option<Job> {
    let mut jobs = queue.jobs.lock().ok()?;
    loop {
        if let Some((_, job)) = jobs.jobs.pop_front() {
            return Some(job);
        }
        if queue.closing.is_cancelled() {
            return jobs.finalizer.take();
        }
        jobs = queue.ready.wait(jobs).ok()?;
    }
}

fn cancel_queued(queue: &Queue, id: u64, error: RocksError) {
    let job = queue.jobs.lock().ok().and_then(|mut state| {
        let index = state
            .jobs
            .iter()
            .position(|(queued_id, _)| *queued_id == id)?;
        state.jobs.remove(index).map(|(_, job)| job)
    });
    if let Some(job) = job {
        job(Err(error));
    }
}

#[cfg(test)]
mod tests;
