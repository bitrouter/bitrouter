//! One retained process lifetime, including startup failures and resumed drains.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, ensure};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::super::engine::Engine;
use super::super::generations::{Attempt, Generations, Report as GenerationReport};
use super::super::ownership::Generation;
use super::super::telemetry::Telemetry;
use super::super::transport::owned::Transport;
use super::health::Health;
use super::workers::Workers;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Withdrawal {
    Unattempted,
    Unknown,
    Confirmed,
    Failed,
}

struct Shutdown {
    deadline: Instant,
    forced: bool,
    withdrawal: Withdrawal,
    failed: bool,
    attempts: Option<Vec<(Generation, Attempt)>>,
    retired: Vec<GenerationReport>,
    telemetry_joined: bool,
    telemetry_clean: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Report {
    pub forced: bool,
    pub withdrawal: Withdrawal,
    pub quiescent: bool,
    pub durable_resolved: bool,
    pub clean: bool,
}

pub(super) struct Lifecycle {
    pub engine: Engine,
    pub generations: Arc<Generations>,
    pub transport: Option<Transport>,
    pub workers: Workers,
    pub health: Arc<Health>,
    shutdown: Option<Shutdown>,
    // All application/worker fields drop before the process exporter owner.
    telemetry: Arc<Telemetry>,
}

impl Lifecycle {
    pub fn new(
        engine: Engine,
        generations: Arc<Generations>,
        workers: Workers,
        health: Arc<Health>,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        Self {
            engine,
            generations,
            transport: None,
            workers,
            health,
            shutdown: None,
            telemetry,
        }
    }

    fn begin(&mut self, deadline: Instant) -> Result<()> {
        if let Some(state) = &self.shutdown {
            ensure!(
                state.deadline == deadline,
                "shared shutdown deadline cannot change"
            );
            return Ok(());
        }
        self.health.withdraw();
        self.generations.withdraw();
        self.workers.stop_early();
        self.shutdown = Some(Shutdown {
            deadline,
            forced: false,
            withdrawal: Withdrawal::Unattempted,
            failed: false,
            attempts: None,
            retired: Vec::new(),
            telemetry_joined: false,
            telemetry_clean: false,
        });
        Ok(())
    }

    fn force(&mut self) {
        if let Some(state) = &mut self.shutdown {
            state.forced = true;
        }
        // Broadcast to every root before awaiting even the first join.
        if let Some(transport) = &self.transport {
            transport.request_force();
        }
        self.generations.request_force();
        self.workers.force();
    }

    pub async fn shutdown_until(
        &mut self,
        deadline: Instant,
        force: &CancellationToken,
    ) -> Result<Report> {
        self.begin(deadline)?;
        if self.report().quiescent {
            return Ok(self.report());
        }
        if self.shutdown.as_ref().is_some_and(|state| !state.forced) && Instant::now() < deadline {
            let graceful = {
                let grace = tokio::time::timeout_at(deadline, self.grace(deadline));
                tokio::select! { biased; () = force.cancelled() => None, result = grace => Some(result) }
            };
            match graceful {
                Some(Ok(Ok(true))) => return Ok(self.report()),
                Some(Ok(Err(_))) => {
                    if let Some(state) = &mut self.shutdown {
                        state.failed = true;
                    }
                    tracing::warn!("shared graceful shutdown retained incomplete resources");
                }
                _ => {}
            }
        }
        self.force();
        loop {
            if self.join_forced().await.is_err() {
                if let Some(state) = &mut self.shutdown {
                    state.failed = true;
                }
                tracing::warn!("shared forced wait retained resources for another observation");
            }
            let report = self.report();
            if report.quiescent {
                return Ok(report);
            }
            // This is observation of the same force attempt, never a new grace
            // budget or SQL cleanup. External cancellation retains this owner.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn grace(&mut self, deadline: Instant) -> Result<bool> {
        let state = self
            .shutdown
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("shutdown not started"))?;
        if state.withdrawal == Withdrawal::Unattempted {
            // Persist uncertainty before polling SQL, including cancellation
            // between COMMIT and receipt of its acknowledgement. Do not replay.
            state.withdrawal = Withdrawal::Unknown;
            state.withdrawal = if self.engine.drain().await.is_ok() {
                Withdrawal::Confirmed
            } else {
                Withdrawal::Failed
            };
        }
        if let Some(transport) = &self.transport {
            transport.stop(deadline)?;
        }
        let generations = &self.generations;
        let workers = &mut self.workers;
        let drain_generations = async {
            workers.join_early().await;
            if state.attempts.is_none() {
                state.attempts = Some(
                    generations
                        .retiring()
                        .into_iter()
                        .map(|id| Ok((id, generations.begin_retirement(id, deadline)?)))
                        .collect::<Result<Vec<_>>>()?,
                );
            }
            let attempts = state.attempts.as_deref().unwrap_or(&[]);
            let waits = attempts.iter().map(|(id, attempt)| async move {
                loop {
                    let report = generations.retire(*id, *attempt).await?;
                    if report.reclaimed || Instant::now() >= deadline {
                        return Ok::<GenerationReport, anyhow::Error>(report);
                    }
                    tokio::time::sleep_until(
                        deadline.min(Instant::now() + Duration::from_millis(25)),
                    )
                    .await;
                }
            });
            state.retired = futures::future::try_join_all(waits).await?;
            Ok::<bool, anyhow::Error>(state.retired.iter().all(|report| report.reclaimed))
        };
        let drain_transport = async {
            match &self.transport {
                Some(transport) => Ok(transport.drain_until(deadline).await?.is_quiescent()),
                None => Ok::<bool, anyhow::Error>(true),
            }
        };
        let (transport, generations) = tokio::try_join!(drain_transport, drain_generations)?;
        if !transport || !generations {
            return Ok(false);
        }
        self.workers.stop_heartbeat().await;
        let telemetry = self.telemetry.drain_until(deadline).await?;
        if let Some(state) = &mut self.shutdown {
            state.telemetry_joined = telemetry.is_quiescent();
            state.telemetry_clean = telemetry.is_clean();
        }
        Ok(telemetry.is_quiescent())
    }

    async fn join_forced(&mut self) -> Result<()> {
        let generations = &self.generations;
        let transport = &self.transport;
        let external = async {
            if let Some(transport) = transport {
                ensure!(
                    transport.join_forced().await?.is_quiescent(),
                    "shared transport is not joined"
                );
            }
            generations.force_pipelines(transport.as_ref())
        };
        let producers = async { generations.join_forced().await.map(|_| ()) };
        let workers = async {
            self.workers.join_forced().await;
            Ok::<(), anyhow::Error>(())
        };
        tokio::try_join!(external, producers, workers)?;
        let all_joined = self
            .generations
            .forced_reports()
            .iter()
            .all(|report| report.is_quiescent())
            && self
                .workers
                .reports()
                .iter()
                .all(|(_, report)| report.joined);
        if all_joined {
            let telemetry = self.telemetry.force_join().await?;
            if let Some(state) = &mut self.shutdown {
                state.telemetry_joined = telemetry.is_quiescent();
                state.telemetry_clean = telemetry.is_clean();
            }
        }
        Ok(())
    }

    pub fn report(&self) -> Report {
        let Some(state) = &self.shutdown else {
            return Report {
                forced: false,
                withdrawal: Withdrawal::Unattempted,
                quiescent: false,
                durable_resolved: false,
                clean: false,
            };
        };
        let forced = self.generations.forced_reports();
        let normal_joined = state.attempts.as_ref().is_some_and(|attempts| {
            state.retired.len() == attempts.len()
                && state.retired.iter().all(|report| report.reclaimed)
        });
        let generations_joined = if state.forced {
            forced.iter().all(|report| report.is_quiescent())
        } else {
            normal_joined
        };
        let durable_resolved = if state.forced {
            forced.iter().all(|report| report.durable_resolved())
        } else {
            normal_joined
        };
        let workers = self.workers.reports();
        let quiescent = generations_joined
            && state.telemetry_joined
            && workers.iter().all(|(_, report)| report.joined)
            && self
                .transport
                .as_ref()
                .is_none_or(|transport| transport.report().is_quiescent());
        let clean = quiescent
            && durable_resolved
            && !state.forced
            && !state.failed
            && state.withdrawal == Withdrawal::Confirmed
            && state.telemetry_clean
            && workers.iter().all(|(_, report)| report.is_clean())
            && state.retired.iter().all(|report| report.is_clean())
            && self
                .transport
                .as_ref()
                .is_none_or(|transport| transport.report().is_clean());
        Report {
            forced: state.forced,
            withdrawal: state.withdrawal,
            quiescent,
            durable_resolved,
            clean,
        }
    }
}
