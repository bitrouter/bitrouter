//! A retained build handle and all resource barriers for one generation.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};

use tokio::task::JoinHandle;

use super::*;
use crate::assemble::{Assembled, SharedAssembly, build_shared_app};
use crate::gateway::pipeline::owned_tasks::{Drain as TaskDrain, OwnedTasks, Summary};
use crate::gateway::shared::http::Snapshot;

pub(super) struct Components {
    // Retain the entire application. In particular, policy/runtime/tracker
    // fields that the HTTP snapshot does not borrow belong to this lifetime.
    pub assembled: Assembled,
    pub snapshot: Option<Arc<Snapshot>>,
    tasks: Option<Arc<OwnedTasks>>,
}

#[derive(Clone)]
struct Outcome {
    components: Option<Arc<Components>>,
    failure: Option<BuildFailure>,
}

enum Build {
    Reserved,
    Running(JoinHandle<Result<Components>>),
    Joined(Outcome),
}

#[derive(Default)]
struct Retirement {
    attempt: Option<Attempt>,
    pipeline: Option<Summary>,
    native: Option<crate::continuation::owned::Drain>,
    producer_closed: bool,
    custody_failed: bool,
}

pub(super) struct Resource {
    pub id: Generation,
    revision: Revision,
    registration: Arc<Registration>,
    build: Mutex<Build>,
    retirement: Mutex<Retirement>,
    retiring: AtomicBool,
    waiting: AtomicBool,
}

pub(super) struct Waiter<'a>(&'a Resource);

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.waiting.store(false, Ordering::Release);
    }
}

impl Resource {
    pub fn new(revision: Revision, registration: Arc<Registration>) -> Self {
        Self {
            id: Generation::new(),
            revision,
            registration,
            build: Mutex::new(Build::Reserved),
            retirement: Mutex::new(Retirement::default()),
            retiring: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
        }
    }

    pub fn producer_id(&self) -> &str {
        self.registration.id()
    }

    pub fn is_retiring(&self) -> bool {
        self.retiring.load(Ordering::Acquire)
    }

    pub fn waiter(&self) -> Result<Waiter<'_>> {
        ensure!(
            self.waiting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "generation already has an active waiter"
        );
        Ok(Waiter(self))
    }

    pub fn start(&self, input: Input, telemetry: Arc<Telemetry>, runtime: &tokio::runtime::Handle) {
        let registration = self.registration.clone();
        let revision = self.revision;
        let id = self.id;
        let mut build = self.build.lock().unwrap_or_else(|error| error.into_inner());
        *build = Build::Running(runtime.spawn(async move {
            let continuation = registration.open().await?;
            let inputs = SharedAssembly {
                continuation,
                telemetry,
                auth_appliers: input.auth_appliers,
                correlation_key: input.correlation_key,
                policies: input.policies,
                policy_lock: input.policy_lock,
            };
            let assembled = build_shared_app(&input.config, &inputs).await?;
            let tasks = assembled
                .app
                .language_model()
                .and_then(|pipeline| pipeline.owned_tasks())
                .cloned();
            let snapshot = match Snapshot::assemble(revision, &assembled.app, id) {
                Ok(snapshot) => Some(snapshot),
                Err(_) => {
                    tracing::error!("Shared candidate could not bind its serving snapshot");
                    None
                }
            };
            Ok(Components {
                assembled,
                snapshot,
                tasks,
            })
        }));
    }

    fn poll_build(&self, cx: &mut TaskContext<'_>) -> Poll<Outcome> {
        let (outcome, observed) = {
            let mut build = self.build.lock().unwrap_or_else(|error| error.into_inner());
            let observed = match &mut *build {
                Build::Reserved => return Poll::Pending,
                Build::Joined(outcome) => return Poll::Ready(outcome.clone()),
                Build::Running(task) => match Pin::new(task).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                },
            };
            let (outcome, discarded) = match observed {
                Ok(Ok(components)) => (
                    Outcome {
                        failure: components
                            .snapshot
                            .is_none()
                            .then_some(BuildFailure::Snapshot),
                        components: Some(Arc::new(components)),
                    },
                    None,
                ),
                other => {
                    let failure = match &other {
                        Err(error) if error.is_panic() => BuildFailure::Panicked,
                        Err(_) => BuildFailure::Cancelled,
                        _ => BuildFailure::Assembly,
                    };
                    (
                        Outcome {
                            components: None,
                            failure: Some(failure),
                        },
                        Some(other),
                    )
                }
            };
            *build = Build::Joined(outcome.clone());
            (outcome, discarded)
        };
        // Panic payloads and construction errors can own arbitrary destructors.
        // Never run them under an ownership mutex.
        drop(observed);
        Poll::Ready(outcome)
    }

    pub fn components(&self) -> Option<Arc<Components>> {
        match &*self.build.lock().unwrap_or_else(|error| error.into_inner()) {
            Build::Joined(outcome) => outcome.components.clone(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn build_task_id(&self) -> Option<tokio::task::Id> {
        match &*self.build.lock().unwrap_or_else(|error| error.into_inner()) {
            Build::Running(task) => Some(task.id()),
            _ => None,
        }
    }

    pub async fn wait_build(&self, deadline: Instant) -> Result<Candidate> {
        let _waiter = self.waiter()?;
        let result =
            tokio::time::timeout_at(deadline, std::future::poll_fn(|cx| self.poll_build(cx))).await;
        Ok(match result {
            Ok(outcome) => Candidate {
                generation: self.id,
                joined: true,
                ready: outcome.failure.is_none() && !self.is_retiring(),
                failure: outcome.failure,
            },
            Err(_) => Candidate {
                generation: self.id,
                joined: false,
                ready: false,
                failure: None,
            },
        })
    }

    pub fn mark_retiring(&self) {
        self.retiring.store(true, Ordering::Release);
        if let Some(components) = self.components()
            && let Some(tasks) = &components.tasks
        {
            tasks.close_sources();
        }
    }

    pub fn begin_attempt(&self, deadline: Instant) -> Result<Attempt> {
        ensure!(self.is_retiring(), "generation has not been withdrawn");
        let mut state = self
            .retirement
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !self.waiting.load(Ordering::Acquire),
            "generation already has an active waiter"
        );
        let attempt = Attempt {
            id: uuid::Uuid::new_v4(),
            deadline,
        };
        state.attempt = Some(attempt);
        Ok(attempt)
    }

    fn report(&self, attempt: Attempt, serving: &Serving) -> Report {
        let (build_joined, build_failure) =
            match &*self.build.lock().unwrap_or_else(|error| error.into_inner()) {
                Build::Joined(outcome) => (true, outcome.failure),
                _ => (false, None),
            };
        let state = self
            .retirement
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Report {
            generation: self.id,
            attempt,
            reclaimed: false,
            build_joined,
            build_failure,
            pipeline: state.pipeline,
            native: state.native,
            producer_closed: state.producer_closed,
            custody: serving.custody.pending_for(self.id),
            custody_failed: state.custody_failed,
        }
    }

    /// The manager keeps both this owner and the waiter guard through removal.
    pub async fn retire(&self, attempt: Attempt, serving: &Serving) -> Result<Report> {
        ensure!(self.is_retiring(), "generation has not been withdrawn");
        ensure!(
            self.retirement
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .attempt
                == Some(attempt),
            "retirement attempt was superseded"
        );
        let deadline = attempt.deadline;
        let outcome =
            match tokio::time::timeout_at(deadline, std::future::poll_fn(|cx| self.poll_build(cx)))
                .await
            {
                Ok(outcome) => outcome,
                Err(_) => return Ok(self.report(attempt, serving)),
            };
        if let Some(components) = &outcome.components {
            let Some(tasks) = &components.tasks else {
                return Ok(self.report(attempt, serving));
            };
            tasks.close_sources();
            match tasks.drain_until(deadline).await? {
                TaskDrain::Joined(summary) => {
                    self.retirement
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pipeline = Some(summary);
                    if summary.forced {
                        return Ok(self.report(attempt, serving));
                    }
                }
                TaskDrain::Retained(_) => return Ok(self.report(attempt, serving)),
            }
            let closed = self
                .retirement
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .producer_closed;
            if !closed && Instant::now() < deadline {
                let drain = components
                    .assembled
                    .continuation_registry
                    .drain_owned_until(deadline)
                    .await?;
                let mut state = self
                    .retirement
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.producer_closed =
                    drain.worker.joined && drain.retained.is_none() && drain.markers.total == 0;
                state.native = Some(drain);
            }
        } else {
            let closed = self
                .retirement
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .producer_closed;
            if !closed && Instant::now() < deadline {
                match tokio::time::timeout_at(deadline, self.registration.close()).await {
                    Ok(Ok(_closure)) => {
                        self.retirement
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .producer_closed = true
                    }
                    Ok(Err(_)) | Err(_) => return Ok(self.report(attempt, serving)),
                }
            }
        }
        self.reconcile_custody(serving, deadline).await;
        let mut report = self.report(attempt, serving);
        let request_barrier = outcome.components.as_ref().is_none_or(|components| {
            let tasks_joined = components.tasks.as_ref().is_some_and(|tasks| {
                let pending = tasks.pending();
                pending.sources == 0 && pending.executions == 0 && pending.settlements == 0
            });
            tasks_joined
                && components
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| serving.retired_quiescent(snapshot))
        });
        report.reclaimed =
            report.build_joined && report.producer_closed && report.custody == 0 && request_barrier;
        Ok(report)
    }

    async fn reconcile_custody(&self, serving: &Serving, deadline: Instant) {
        // A busy chunk is not an idle sweep: old entries may be behind live
        // requests from the current generation. Bound each cooperative batch,
        // visit a complete initial queue, and repeat only after real progress.
        // Cancellation preserves the registry's own RAII reconciliation owner.
        let mut remaining = serving.custody.pending();
        let mut progressed = false;
        let mut batch = 0;
        while remaining > 0 && Instant::now() < deadline && serving.custody.pending_for(self.id) > 0
        {
            match tokio::time::timeout_at(deadline, serving.custody.reconcile_one()).await {
                Ok(Ok(released)) => progressed |= released,
                Ok(Err(_)) => {
                    self.retirement
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .custody_failed = true;
                    return;
                }
                Err(_) => return,
            }
            remaining -= 1;
            batch += 1;
            if remaining == 0 && progressed {
                remaining = serving.custody.pending();
                progressed = false;
            }
            if batch == 32 {
                tokio::task::yield_now().await;
                batch = 0;
            }
        }
    }
}

impl Drop for Resource {
    fn drop(&mut self) {
        if let Build::Running(task) = self
            .build
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
        {
            task.abort();
            tracing::error!("Generation resource dropped before its build joined");
        }
    }
}
