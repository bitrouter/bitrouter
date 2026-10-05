//! Shared local operator actions for the CLI and coding TUI.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::control::{BlockDefinition, ControlState, EvolutionMode};
use super::jobs::{JobStatus, JudgeJobs};
use super::learning::LearningReport;
use super::runtime::EvolutionRuntime;
use super::scheduler::{EvolutionScheduler, SessionSchedule, WorkerStatus};

pub mod checkpoint;

#[derive(Debug, clap::Subcommand)]
pub enum EvolutionAction {
    /// Inspect modes, blocks, scheduled checkpoints and judge task status.
    Status,
    /// Select feedback mode; automatic requires a configured judge model.
    Mode {
        #[arg(value_enum)]
        mode: EvolutionMode,
        #[arg(long)]
        judge_model: Option<String>,
    },
    /// Register a complete policy-block experiment from a JSON definition.
    Register { file: PathBuf },
    /// Start the next experiment for the same block; retain the previous evidence.
    Revise {
        file: PathBuf,
        #[arg(long)]
        expected_experiment: String,
    },
    /// Withdraw the current candidate and return to its supported baseline.
    Restore {
        block: String,
        #[arg(long)]
        expected_experiment: String,
        #[arg(long)]
        expected_revision: String,
        #[arg(long)]
        reason: String,
    },
    /// Inspect the current evidence and proposed allocation without publishing.
    Learning {
        block: String,
        #[arg(long)]
        experiment: Option<String>,
    },
    /// Reconcile current evidence against live routes and publish an eligible change.
    Improve {
        block: String,
        #[arg(long)]
        experiment: Option<String>,
    },
}

impl EvolutionAction {
    pub async fn operation(self) -> Result<EvolutionOperation> {
        Ok(match self {
            Self::Status => EvolutionOperation::Status,
            Self::Mode { mode, judge_model } => EvolutionOperation::Mode { mode, judge_model },
            Self::Register { file } => EvolutionOperation::Register {
                definition: Box::new(serde_json::from_str(
                    &tokio::fs::read_to_string(&file)
                        .await
                        .with_context(|| format!("reading block definition {}", file.display()))?,
                )?),
            },
            Self::Revise {
                file,
                expected_experiment,
            } => EvolutionOperation::Revise {
                definition: Box::new(serde_json::from_str(
                    &tokio::fs::read_to_string(&file)
                        .await
                        .with_context(|| format!("reading block revision {}", file.display()))?,
                )?),
                expected_experiment,
            },
            Self::Learning { block, experiment } => {
                EvolutionOperation::Learning { block, experiment }
            }
            Self::Restore {
                block,
                expected_experiment,
                expected_revision,
                reason,
            } => EvolutionOperation::Restore {
                request: super::control::restoration::RestoreRequest {
                    block,
                    expected_experiment,
                    expected_revision,
                    reason,
                },
            },
            Self::Improve { block, experiment } => {
                EvolutionOperation::Improve { block, experiment }
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum EvolutionOperation {
    Candidate {
        action: super::runtime::candidates::CandidateAction,
    },
    JudgeModel {
        model: String,
    },
    Checkpoint {
        source: String,
        session_id: String,
        action: checkpoint::CheckpointAction,
    },
    Status,
    Restore {
        request: super::control::restoration::RestoreRequest,
    },
    Mode {
        mode: EvolutionMode,
        judge_model: Option<String>,
    },
    Register {
        definition: Box<BlockDefinition>,
    },
    Revise {
        definition: Box<BlockDefinition>,
        expected_experiment: String,
    },
    Learning {
        block: String,
        #[serde(default)]
        experiment: Option<String>,
    },
    Improve {
        block: String,
        #[serde(default)]
        experiment: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    pub source: String,
    pub session_id: String,
    pub checkpoint_id: String,
    pub status: JobStatus,
    pub attempted_requests: usize,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionStatus {
    pub control: ControlState,
    pub worker: WorkerStatus,
    pub schedules: Vec<SessionSchedule>,
    pub jobs: Vec<JobSummary>,
    pub judge_costs: super::costs::report::JudgeCostReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "report", rename_all = "snake_case")]
pub enum EvolutionReport {
    Candidate(Box<super::runtime::candidates::CandidateReport>),
    Status(Box<EvolutionStatus>),
    Learning(Box<LearningReport>),
    Checkpoint(Box<checkpoint::CheckpointReport>),
}

impl EvolutionRuntime {
    pub async fn operate(
        &self,
        owner: &str,
        operation: EvolutionOperation,
    ) -> Result<EvolutionReport> {
        let service = self.service(owner)?;
        match operation {
            EvolutionOperation::Candidate { action } => {
                return Ok(EvolutionReport::Candidate(Box::new(
                    self.candidate_action(owner, action).await?,
                )));
            }
            EvolutionOperation::Checkpoint {
                source,
                session_id,
                action,
            } => {
                return Ok(EvolutionReport::Checkpoint(Box::new(
                    checkpoint::operate(&self.db, owner, source, session_id, action).await?,
                )));
            }
            EvolutionOperation::Status => {}
            EvolutionOperation::Restore { request } => {
                service.restore(&request).await?;
            }
            EvolutionOperation::Mode { mode, judge_model } => {
                service.set_mode(mode, judge_model).await?;
            }
            EvolutionOperation::JudgeModel { model } => {
                service.set_judge_model(model).await?;
            }
            EvolutionOperation::Register { definition } => {
                self.register(owner, *definition).await?;
            }
            EvolutionOperation::Revise {
                definition,
                expected_experiment,
            } => {
                self.revise(owner, *definition, expected_experiment).await?;
            }
            EvolutionOperation::Learning { block, experiment } => {
                return Ok(EvolutionReport::Learning(Box::new(
                    service
                        .learning_status_experiment(&block, experiment.as_deref())
                        .await?,
                )));
            }
            EvolutionOperation::Improve { block, experiment } => {
                return Ok(EvolutionReport::Learning(Box::new(
                    self.reconcile_experiment(owner, &block, experiment.as_deref())
                        .await?,
                )));
            }
        }
        let recorded_jobs = JudgeJobs::new(service.store.clone()).list().await?;
        let judge_costs = super::costs::JudgeCosts::new(self.db.clone())
            .report(owner, &recorded_jobs)
            .await?;
        let jobs = recorded_jobs
            .into_iter()
            .map(|job| JobSummary {
                job_id: job.job_id,
                source: job.identity.source,
                session_id: job.identity.native_session_id,
                checkpoint_id: job.checkpoint_id,
                status: job.status,
                attempted_requests: job.request_ids.len(),
                error_code: job.error_code,
            })
            .collect();
        Ok(EvolutionReport::Status(Box::new(EvolutionStatus {
            control: service.state().await?,
            worker: self.worker_status(),
            schedules: EvolutionScheduler::new(self.clone())
                .schedules(owner)
                .await?,
            jobs,
            judge_costs,
        })))
    }
}

impl crate::output::CliReport for EvolutionReport {
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        human.line(&serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)
    }
}
