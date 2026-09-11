//! Durable, leased judge jobs. A cached response is reused after a process restart.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::language_model::pipeline::Pipeline;
use chrono::{DateTime, Utc};
use sea_orm::{DatabaseTransaction, TransactionTrait};
use serde::{Deserialize, Serialize};

use super::{judge, rubric::digest, scoring, store::EvolutionStore};
use crate::acp_trajectory::checkpoint::types::AssessmentSource;
use crate::acp_trajectory::{CanonicalStore, SessionIdentity};

const KIND: &str = "judge_job";
const MAX_ATTEMPTS: usize = 3;

async fn check_mode(
    store: &EvolutionStore,
    tx: &DatabaseTransaction,
    epoch: Option<u64>,
    model: &str,
) -> Result<()> {
    if let Some(epoch) = epoch {
        let (_, state): (_, super::control::ControlState) = store
            .lock(
                tx,
                super::service::CONTROL_KIND,
                super::service::CONTROL_KEY,
            )
            .await?;
        ensure!(
            state.mode == super::control::EvolutionMode::Automatic
                && state.mode_epoch == epoch
                && state.judge_model.as_deref() == Some(model),
            "automatic judge job belongs to an inactive feedback epoch"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    ResponseStored,
    Completed,
    Failed,
    Superseded,
}

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeJob {
    pub job_id: String,
    pub identity: SessionIdentity,
    pub checkpoint_id: String,
    pub expected_revision: Option<String>,
    pub model: String,
    pub input_digest: String,
    pub mode_epoch: Option<u64>,
    pub status: JobStatus,
    pub lease_owner: Option<String>,
    pub lease_until: Option<String>,
    /// Unique requests, including uncertain or failed attempts; never silently
    /// reuse an upstream request ID as a promise of provider idempotency.
    pub request_ids: Vec<String>,
    pub cached_response: Option<judge::JudgeOutput>,
    pub outcome_revision: Option<String>,
    pub error_code: Option<String>,
}

impl JudgeJob {
    pub(crate) fn lease_live(&self) -> Result<bool> {
        Ok(self
            .lease_until
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose()?
            .is_some_and(|deadline| deadline > Utc::now()))
    }

    pub(crate) fn ready(&self) -> Result<bool> {
        Ok(
            !matches!(self.status, JobStatus::Completed | JobStatus::Superseded)
                && !self.lease_live()?
                && (self.cached_response.is_some() || self.request_ids.len() < MAX_ATTEMPTS),
        )
    }
}

#[derive(Clone)]
pub struct JudgeJobs {
    store: EvolutionStore,
    canonical: CanonicalStore,
}

impl JudgeJobs {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            canonical: CanonicalStore::new(store.db.clone()),
            store,
        }
    }

    pub async fn enqueue(
        &self,
        identity: &SessionIdentity,
        checkpoint: &str,
        model: &str,
        mode_epoch: Option<u64>,
    ) -> Result<JudgeJob> {
        self.store.ensure_owner(&identity.owner)?;
        let input = scoring::prepare(&self.canonical, identity, checkpoint).await?;
        let input_digest = judge::input_digest(&input, model)?;
        let job_id = digest(&(
            identity,
            checkpoint,
            model,
            mode_epoch,
            judge::JUDGE_VERSION,
        ))?;
        let job = JudgeJob {
            job_id: job_id.clone(),
            identity: identity.clone(),
            checkpoint_id: checkpoint.into(),
            expected_revision: input.expected_revision,
            model: model.into(),
            input_digest,
            mode_epoch,
            status: JobStatus::Queued,
            lease_owner: None,
            lease_until: None,
            request_ids: vec![],
            cached_response: None,
            outcome_revision: None,
            error_code: None,
        };
        let tx = self.store.db.begin().await?;
        check_mode(&self.store, &tx, mode_epoch, model).await?;
        let (_, stored): (_, JudgeJob) = self
            .store
            .initialize_in(&tx, KIND, &job_id, Some(identity.key()?), &job)
            .await?;
        ensure!(
            stored.input_digest == job.input_digest && stored.identity == *identity,
            "judge job identity collision"
        );
        tx.commit().await?;
        Ok(stored)
    }

    pub async fn get(&self, id: &str) -> Result<JudgeJob> {
        let (_, job): (_, JudgeJob) = self
            .store
            .get(KIND, id)
            .await?
            .context("judge job not found")?;
        self.canonical
            .checkpoint_content(&job.identity, &job.checkpoint_id)
            .await?;
        Ok(job)
    }

    pub async fn list(&self) -> Result<Vec<JudgeJob>> {
        Ok(self
            .store
            .list::<JudgeJob>(KIND)
            .await?
            .into_iter()
            .map(|(_, _, job)| job)
            .collect())
    }

    /// Retire automatic work whose immutable input or feedback epoch is obsolete.
    /// A live lease still owns its response; its final selection is separately fenced.
    pub(crate) async fn supersede(&self, id: &str, code: &str) -> Result<()> {
        let (revision, job): (_, JudgeJob) = self
            .store
            .get(KIND, id)
            .await?
            .context("judge job missing")?;
        if matches!(job.status, JobStatus::Completed | JobStatus::Superseded) || job.lease_live()? {
            return Ok(());
        }
        self.store
            .update(KIND, id, revision, |job: &mut JudgeJob| {
                ensure!(!job.lease_live()?, "judge job became leased");
                job.status = JobStatus::Superseded;
                job.error_code = Some(code.into());
                job.lease_owner = None;
                job.lease_until = None;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Repair a crash after the canonical revision committed but before the job
    /// receipt committed. This records an existing result, never selects a label.
    pub(crate) async fn recover_completed(&self, id: &str) -> Result<Option<JudgeJob>> {
        let (version, job): (_, JudgeJob) = self
            .store
            .get(KIND, id)
            .await?
            .context("judge job missing")?;
        if job.status == JobStatus::Completed {
            return Ok(Some(self.get(id).await?));
        }
        if job.cached_response.is_none() || job.lease_live()? {
            return Ok(None);
        }
        let input = scoring::prepare(&self.canonical, &job.identity, &job.checkpoint_id).await?;
        if judge::input_digest(&input, &job.model)? != job.input_digest {
            // Historical committed revisions remain readable. A new evaluator
            // must not reinterpret a pending old-contract response to recover it.
            return Ok(None);
        }
        let selected = self
            .canonical
            .assessment_history(&job.identity)
            .await?
            .into_iter()
            .find(|revision| revision.input.submission_id == format!("judge:{}", job.job_id));
        let Some(receipt) = selected else {
            return Ok(None);
        };
        ensure!(
            receipt.input.checkpoint_id == job.checkpoint_id
                && receipt.input.source == AssessmentSource::Agentic
                && receipt.input.evaluator_id == format!("checkpoint-judge:{}", job.model)
                && receipt.input.evaluator_version == judge::JUDGE_VERSION,
            "judge receipt identity mismatch"
        );
        let response = job
            .cached_response
            .as_ref()
            .context("cached judge response missing")?;
        ensure!(
            response.input_digest == job.input_digest
                && response.model == job.model
                && response.judge_version == judge::JUDGE_VERSION
                && job.request_ids.last() == Some(&response.request_id),
            "cached judge response does not match the completed attempt"
        );
        let contract = scoring::measurement_contract(
            AssessmentSource::Agentic,
            &receipt.input.evaluator_id,
            &receipt.input.evaluator_version,
        )?;
        let expected = response.evaluation.assessment(&input.evidence, &contract)?;
        ensure!(
            receipt.input.assessment.as_ref().map(digest).transpose()? == Some(digest(&expected)?),
            "committed assessment differs from the cached judge response"
        );
        self.store
            .update(KIND, id, version, |job: &mut JudgeJob| {
                ensure!(!job.lease_live()?, "judge job became leased");
                job.status = JobStatus::Completed;
                job.outcome_revision = Some(receipt.revision_id);
                job.error_code = None;
                job.lease_owner = None;
                job.lease_until = None;
                Ok(())
            })
            .await?;
        Ok(Some(self.get(id).await?))
    }

    async fn owned_update<R>(
        &self,
        id: &str,
        owner: &str,
        update: impl FnOnce(&mut JudgeJob) -> Result<R>,
    ) -> Result<R> {
        let (revision, _): (_, JudgeJob) = self
            .store
            .get(KIND, id)
            .await?
            .context("judge job was deleted")?;
        let (_, result) = self
            .store
            .update(KIND, id, revision, |job: &mut JudgeJob| {
                ensure!(
                    job.lease_owner.as_deref() == Some(owner),
                    "judge worker lease was superseded"
                );
                update(job)
            })
            .await?;
        Ok(result)
    }

    /// Claim and final assessment selection both check automatic jobs' epochs
    /// under the same control lock used by mode changes. An already issued call
    /// may finish and be cached, but cannot select an assessment after being
    /// fenced out. Explicit one-shot jobs have no automatic epoch.
    pub async fn run(&self, id: &str, pipeline: &Arc<Pipeline>) -> Result<JudgeJob> {
        if let Some(completed) = self.recover_completed(id).await? {
            return Ok(completed);
        }
        let (revision, existing): (_, JudgeJob) = self
            .store
            .get(KIND, id)
            .await?
            .context("judge job not found")?;
        self.canonical
            .checkpoint_content(&existing.identity, &existing.checkpoint_id)
            .await?;
        if existing.status == JobStatus::Completed {
            return Ok(existing);
        }
        ensure!(
            existing.status != JobStatus::Superseded,
            "judge input was superseded; evaluate a current checkpoint"
        );
        let input =
            scoring::prepare(&self.canonical, &existing.identity, &existing.checkpoint_id).await?;
        if judge::input_digest(&input, &existing.model)? != existing.input_digest {
            self.supersede(id, "judge_contract_superseded").await?;
            anyhow::bail!(
                "judge contract changed; explicitly judge the checkpoint with the current evaluator"
            );
        }
        let owner = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let tx = self.store.db.begin().await?;
        check_mode(&self.store, &tx, existing.mode_epoch, &existing.model).await?;
        let (row, mut job): (_, JudgeJob) = self.store.lock(&tx, KIND, id).await?;
        ensure!(
            row.revision == revision,
            "judge job changed before claim; refresh before retrying"
        );
        let lease_live = job
            .lease_until
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose()?
            .is_some_and(|deadline| deadline > now);
        ensure!(!lease_live, "judge job is already leased by a live worker");
        ensure!(
            job.cached_response.is_some() || job.request_ids.len() < MAX_ATTEMPTS,
            "judge retry limit reached; inspect the job before starting a new evaluation"
        );
        job.lease_owner = Some(owner.clone());
        job.lease_until = Some((now + chrono::Duration::minutes(2)).to_rfc3339());
        if job.cached_response.is_none() {
            let request_id = format!("brjudge_{}", uuid::Uuid::new_v4().simple());
            super::costs::JudgeCosts::new(self.store.db.clone())
                .reserve(&tx, &job, &request_id)
                .await?;
            job.request_ids.push(request_id);
            job.status = JobStatus::Running;
        }
        self.store.save(&tx, row, &job).await?;
        tx.commit().await?;
        let result = self.run_claimed(&mut job, pipeline, &owner).await;
        if result.is_err() {
            // Provider errors can contain response fragments. Persist only an
            // operational code, not potentially quoted or secret response text.
            let _ = self
                .owned_update(id, &owner, |job| {
                    job.status = JobStatus::Failed;
                    job.error_code = Some("judge_execution_or_submission_failed".into());
                    job.lease_owner = None;
                    job.lease_until = None;
                    Ok(())
                })
                .await;
        }
        result?;
        self.get(id).await
    }

    async fn run_claimed(
        &self,
        job: &mut JudgeJob,
        pipeline: &Arc<Pipeline>,
        owner: &str,
    ) -> Result<()> {
        if job.cached_response.is_none() {
            let mut input =
                scoring::prepare(&self.canonical, &job.identity, &job.checkpoint_id).await?;
            // Compare-and-set is against the original selection, even if a
            // correction arrived while the model was offline.
            input.expected_revision = job.expected_revision.clone();
            ensure!(
                judge::input_digest(&input, &job.model)? == job.input_digest,
                "judge input changed"
            );
            let request_id = job
                .request_ids
                .last()
                .context("judge attempt identity missing")?;
            let response = {
                let future = judge::evaluate(pipeline, &job.model, request_id, &input);
                let renewal = self.renew_lease(&job.job_id, owner);
                tokio::pin!(future, renewal);
                // Both futures remain polled while waiting for database I/O.
                // Awaiting renewal inside a selected branch could suspend a
                // pipeline hook that holds the pool's only connection. Drop
                // the losing future here, before taking another store lock.
                tokio::select! {
                    response = &mut future => response?,
                    result = &mut renewal => {
                        result?;
                        anyhow::bail!("judge lease renewal stopped")
                    }
                }
            };
            self.canonical
                .checkpoint_content(&job.identity, &job.checkpoint_id)
                .await?;
            self.owned_update(&job.job_id, owner, |job| {
                job.cached_response = Some(response.clone());
                job.status = JobStatus::ResponseStored;
                Ok(())
            })
            .await?;
            job.cached_response = Some(response);
        }
        let response = job
            .cached_response
            .as_ref()
            .context("judge response missing")?;
        ensure!(
            response.input_digest == job.input_digest
                && response.model == job.model
                && response.judge_version == judge::JUDGE_VERSION
                && job.request_ids.last() == Some(&response.request_id),
            "cached judge response does not match the leased input and attempt"
        );
        let epoch = job.mode_epoch;
        let model = job.model.clone();
        let store = self.store.clone();
        let receipt = scoring::submit_checked(
            &self.canonical,
            &job.identity,
            scoring::RubricSubmission {
                submission_id: format!("judge:{}", job.job_id),
                checkpoint_id: job.checkpoint_id.clone(),
                expected_revision: job.expected_revision.clone(),
                source: AssessmentSource::Agentic,
                evaluator_id: format!("checkpoint-judge:{}", job.model),
                evaluator_version: judge::JUDGE_VERSION.into(),
                evaluation: response.evaluation.clone(),
            },
            move |tx| Box::pin(async move { check_mode(&store, tx, epoch, &model).await }),
        )
        .await?;
        self.owned_update(&job.job_id, owner, |job| {
            job.outcome_revision = Some(receipt.revision.revision_id);
            job.status = JobStatus::Completed;
            job.error_code = None;
            job.lease_owner = None;
            job.lease_until = None;
            Ok(())
        })
        .await?;
        Ok(())
    }

    async fn renew_lease(&self, id: &str, owner: &str) -> Result<()> {
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            heartbeat.tick().await;
            self.owned_update(id, owner, |job| {
                job.lease_until = Some((Utc::now() + chrono::Duration::minutes(2)).to_rfc3339());
                Ok(())
            })
            .await?;
        }
    }
}
