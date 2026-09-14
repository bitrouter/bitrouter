//! Restartable discovery of stopped ACP prefixes and automatic feedback work.
//!
//! Only the daemon owns this loop. Capture forwards independently; evaluation
//! never runs inside a transport append or blocks the coding prompt's response.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::acp::capture::{CaptureEvent, CaptureKind};
use bitrouter_sdk::language_model::pipeline::Pipeline;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::control::{BlockStatus, ControlState, EvolutionMode};
use super::jobs::{JobStatus, JudgeJobs};
use super::runtime::EvolutionRuntime;
use super::service::{CONTROL_KEY, CONTROL_KIND, EvolutionService};
use super::store::EvolutionStore;
use crate::acp_trajectory::{
    CanonicalStore, SessionIdentity,
    entities::{events, sessions},
};

const CURSOR_KIND: &str = "checkpoint_schedule";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSchedule {
    pub identity: SessionIdentity,
    pub mode_epoch: u64,
    pub scanned_watermark: i64,
    pub checkpoint_id: Option<String>,
    pub judge_job_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassReport {
    pub checkpoints_created: usize,
    pub jobs_completed: usize,
    pub publications: usize,
    /// Scoped operational codes only; no model responses or transport secrets.
    pub errors: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub running: bool,
    pub last_started_at: Option<String>,
    pub last_finished_at: Option<String>,
    pub last_report: Option<PassReport>,
}

#[derive(Clone)]
pub struct EvolutionScheduler {
    runtime: EvolutionRuntime,
}

async fn feedback_epoch(
    store: &EvolutionStore,
    tx: &DatabaseTransaction,
    expected: &ControlState,
) -> Result<()> {
    let (_, current): (_, ControlState) = store.lock(tx, CONTROL_KIND, CONTROL_KEY).await?;
    ensure!(
        current.mode != EvolutionMode::Off
            && current.mode == expected.mode
            && current.mode_epoch == expected.mode_epoch
            && current.judge_model == expected.judge_model,
        "feedback epoch changed during checkpoint discovery"
    );
    Ok(())
}

impl EvolutionScheduler {
    pub fn new(runtime: EvolutionRuntime) -> Self {
        Self { runtime }
    }

    fn status(&self, update: impl FnOnce(&mut WorkerStatus)) {
        let mut status = match self.runtime.worker_status.lock() {
            Ok(status) => status,
            Err(poisoned) => poisoned.into_inner(),
        };
        update(&mut status);
    }

    pub async fn schedules(&self, owner: &str) -> Result<Vec<SessionSchedule>> {
        Ok(EvolutionStore::new(self.runtime.db.clone(), owner)?
            .list::<SessionSchedule>(CURSOR_KIND)
            .await?
            .into_iter()
            .map(|(_, _, schedule)| schedule)
            .collect())
    }

    /// The caller owns cancellation and joins this future at daemon shutdown.
    /// Dropping an interrupted model attempt leaves its durable lease/request
    /// identity for restart recovery; it does not pretend the call was free.
    pub async fn run(&self, pipeline: Arc<Pipeline>, shutdown: CancellationToken) {
        self.status(|status| status.running = true);
        let mut clock = tokio::time::interval(std::time::Duration::from_secs(2));
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_errors = BTreeMap::new();
        loop {
            tokio::select! { _ = shutdown.cancelled() => break, _ = clock.tick() => {} }
            let result = tokio::select! {
                _ = shutdown.cancelled() => break,
                result = self.tick(&pipeline) => result,
            };
            let errors = match result {
                Ok(report) => report.errors,
                Err(_) => {
                    let errors =
                        BTreeMap::from([("worker".into(), "checkpoint_discovery_failed".into())]);
                    self.status(|status| {
                        status.last_finished_at = Some(chrono::Utc::now().to_rfc3339());
                        status.last_report = Some(PassReport {
                            errors: errors.clone(),
                            ..Default::default()
                        });
                    });
                    errors
                }
            };
            if errors != last_errors && !errors.is_empty() {
                tracing::warn!(?errors, "checkpoint evolution has unfinished work");
            }
            last_errors = errors;
        }
        self.status(|status| status.running = false);
    }

    /// One bounded discovery pass; all retries use persisted input/job identity.
    /// A judge request can take longer than the polling interval, with no product
    /// token or cost ceiling. Work remains outside the coding request pipeline.
    pub async fn tick(&self, pipeline: &Arc<Pipeline>) -> Result<PassReport> {
        self.status(|status| status.last_started_at = Some(chrono::Utc::now().to_rfc3339()));
        let mut owners = BTreeMap::<String, Vec<sessions::Model>>::new();
        for row in sessions::Entity::find()
            .filter(sessions::Column::Deleted.eq(false))
            .all(&self.runtime.db)
            .await?
        {
            owners.entry(row.owner.clone()).or_default().push(row);
        }
        let mut report = PassReport::default();
        for (owner, rows) in owners {
            let service = self.runtime.service(&owner)?;
            let state = service.state().await?;
            if state.mode != EvolutionMode::Off {
                for row in rows {
                    match self.discover(&service, &state, &row).await {
                        Ok(true) => report.checkpoints_created += 1,
                        Ok(false) => {}
                        Err(_) => {
                            report.errors.insert(
                                row.session_key,
                                "checkpoint_discovery_retry_required".into(),
                            );
                        }
                    }
                }
            }
            self.judges(&service, pipeline, &mut report).await?;
            let current = service.state().await?;
            if current.mode != EvolutionMode::Off {
                if self.refresh_resources(&service).await.is_err() {
                    report
                        .errors
                        .insert(owner.clone(), "resource_refresh_retry_required".into());
                    continue;
                }
                for block in current
                    .blocks
                    .values()
                    .chain(current.archived_experiments.values().map(|a| &a.block))
                {
                    if !matches!(block.status, BlockStatus::Exploring | BlockStatus::Adopted) {
                        continue;
                    }
                    let id = &block.definition.block_id;
                    match self
                        .runtime
                        .reconcile_experiment(&owner, id, Some(&block.experiment_id))
                        .await
                    {
                        Ok(learning) => report.publications += usize::from(learning.published),
                        Err(_) => {
                            report.errors.insert(
                                format!("{owner}:{id}:{}", block.experiment_id),
                                "learning_or_route_validation_retry_required".into(),
                            );
                        }
                    }
                }
            }
        }
        self.status(|status| {
            status.last_finished_at = Some(chrono::Utc::now().to_rfc3339());
            status.last_report = Some(report.clone());
        });
        Ok(report)
    }

    async fn discover(
        &self,
        service: &EvolutionService,
        state: &ControlState,
        row: &sessions::Model,
    ) -> Result<bool> {
        let previous = service
            .store
            .get::<SessionSchedule>(CURSOR_KIND, &row.session_key)
            .await?
            .map(|(_, schedule)| schedule);
        if previous.as_ref().is_some_and(|cursor| {
            cursor.mode_epoch == state.mode_epoch && cursor.scanned_watermark == row.head
        }) {
            return Ok(false);
        }
        let since = state
            .feedback_started_at
            .as_deref()
            .context("feedback discovery epoch has no start time")?;
        let since = chrono::DateTime::parse_from_rfc3339(since)?;
        let mut open = BTreeSet::new();
        let mut stopped = false;
        for raw in events::Entity::find()
            .filter(events::Column::SessionKey.eq(&row.session_key))
            .filter(events::Column::SessionSequence.lte(row.head))
            .filter(events::Column::Replay.eq(false))
            .order_by_asc(events::Column::SessionSequence)
            .all(&self.runtime.db)
            .await?
        {
            let event: CaptureEvent = serde_json::from_str(&raw.event_json)?;
            if event.method != "session/prompt" {
                continue;
            }
            let call = event
                .call_id
                .context("recorded prompt has no call identity")?;
            let key = (raw.connection_id, call);
            match event.kind {
                CaptureKind::Request => {
                    open.insert(key);
                }
                CaptureKind::Response => {
                    let matched = open.remove(&key);
                    stopped =
                        matched && chrono::DateTime::parse_from_rfc3339(&raw.captured_at)? >= since;
                }
                _ => {}
            }
        }
        let identity = SessionIdentity {
            owner: row.owner.clone(),
            source: row.source.clone(),
            native_session_id: row.native_session_id.clone(),
        };
        let mut cursor = previous
            .filter(|cursor| cursor.mode_epoch == state.mode_epoch)
            .unwrap_or_else(|| SessionSchedule {
                identity: identity.clone(),
                mode_epoch: state.mode_epoch,
                scanned_watermark: 0,
                checkpoint_id: None,
                judge_job_id: None,
                updated_at: String::new(),
            });
        let ready = stopped && open.is_empty();
        if ready {
            let canonical = CanonicalStore::new(self.runtime.db.clone());
            let expected = state.clone();
            let store = service.store.clone();
            let cp = canonical
                .freeze_checkpoint_checked(&identity, row.head, move |tx| {
                    Box::pin(async move { feedback_epoch(&store, tx, &expected).await })
                })
                .await?;
            cursor.checkpoint_id = Some(cp.checkpoint_id.clone());
            cursor.judge_job_id = None;
            let effective = canonical.effective_assessment(&identity).await?;
            if state.mode == EvolutionMode::Automatic
                && (effective.stale || effective.assessment.is_none())
            {
                let model = state
                    .judge_model
                    .as_deref()
                    .context("automatic judge model missing")?;
                let job = JudgeJobs::new(service.store.clone())
                    .enqueue(&identity, &cp.checkpoint_id, model, Some(state.mode_epoch))
                    .await?;
                cursor.judge_job_id = Some(job.job_id);
            }
        }
        cursor.scanned_watermark = row.head;
        cursor.updated_at = chrono::Utc::now().to_rfc3339();
        let tx = self.runtime.db.begin().await?;
        feedback_epoch(&service.store, &tx, state).await?;
        sessions::Entity::update_many()
            .col_expr(
                sessions::Column::Head,
                Expr::col(sessions::Column::Head).into(),
            )
            .filter(sessions::Column::SessionKey.eq(&row.session_key))
            .exec(&tx)
            .await?;
        ensure!(
            sessions::Entity::find_by_id(&row.session_key)
                .one(&tx)
                .await?
                .as_ref()
                == Some(row),
            "canonical head changed during checkpoint discovery"
        );
        service
            .store
            .initialize_in(
                &tx,
                CURSOR_KIND,
                &row.session_key,
                Some(row.session_key.clone()),
                &cursor,
            )
            .await?;
        let (stored_row, _): (_, SessionSchedule) = service
            .store
            .lock(&tx, CURSOR_KIND, &row.session_key)
            .await?;
        service.store.save(&tx, stored_row, &cursor).await?;
        tx.commit().await?;
        Ok(ready)
    }

    async fn judges(
        &self,
        service: &EvolutionService,
        pipeline: &Arc<Pipeline>,
        report: &mut PassReport,
    ) -> Result<()> {
        let jobs = JudgeJobs::new(service.store.clone());
        let canonical = CanonicalStore::new(self.runtime.db.clone());
        for job in jobs.list().await? {
            // Explicit one-shot CLI evaluations are never adopted by this worker.
            let Some(epoch) = job.mode_epoch else {
                continue;
            };
            if matches!(job.status, JobStatus::Completed | JobStatus::Superseded) {
                continue;
            }
            let result: Result<bool> = async {
                if jobs.recover_completed(&job.job_id).await?.is_some() {
                    return Ok(false);
                }
                let state = service.state().await?;
                let code = if state.mode != EvolutionMode::Automatic
                    || state.mode_epoch != epoch
                    || state.judge_model.as_ref() != Some(&job.model)
                {
                    Some("feedback_epoch_superseded")
                } else {
                    let content = canonical
                        .checkpoint_content(&job.identity, &job.checkpoint_id)
                        .await?;
                    let effective = canonical.effective_assessment(&job.identity).await?;
                    if effective.current_watermark != content.checkpoint.watermark {
                        Some("canonical_prefix_superseded")
                    } else if effective.current_revision != job.expected_revision {
                        Some("assessment_revision_superseded")
                    } else {
                        None
                    }
                };
                if let Some(code) = code {
                    jobs.supersede(&job.job_id, code).await?;
                    return Ok(false);
                }
                if !job.ready()? {
                    ensure!(job.lease_live()?, "automatic judge retry limit reached");
                    return Ok(false);
                }
                jobs.run(&job.job_id, pipeline).await?;
                Ok(true)
            }
            .await;
            match result {
                Ok(completed) => report.jobs_completed += usize::from(completed),
                Err(_) => {
                    report
                        .errors
                        .insert(job.job_id, "judge_execution_or_submission_failed".into());
                }
            }
        }
        Ok(())
    }

    async fn refresh_resources(&self, service: &EvolutionService) -> Result<()> {
        let canonical = CanonicalStore::new(self.runtime.db.clone());
        for (_, _, enrollment) in service
            .store
            .list::<super::service::SessionEnrollment>(super::service::SESSION_KIND)
            .await?
        {
            let row = sessions::Entity::find_by_id(enrollment.identity.key()?)
                .one(&self.runtime.db)
                .await?;
            if row.is_none_or(|row| row.deleted) {
                continue;
            }
            let effective = canonical.effective_assessment(&enrollment.identity).await?;
            if !effective.stale
                && let Some(cp) = effective.checkpoint
            {
                canonical
                    .observe_checkpoint_resources(&enrollment.identity, &cp.checkpoint_id)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
