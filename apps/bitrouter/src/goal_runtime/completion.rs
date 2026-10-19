use super::*;
use bitrouter_orchestrator::goal::completion::{self, Attempt, Event, State, Verdict};
use bitrouter_orchestrator::goal::verification::CheckOutput;
use bitrouter_orchestrator::goal::workflow::CheckState;
mod evidence;

impl Coordinator {
    pub(super) async fn drive_completion(&self, goal: &GoalSnapshot) -> Result<()> {
        let progress = &goal.workflow.completion;
        if progress.completed_for(goal.requirements_revision) {
            return Ok(());
        }
        if progress
            .attempt
            .as_ref()
            .is_none_or(|attempt| attempt.requirements_revision != goal.requirements_revision)
        {
            if goal.run_intent != RunIntent::Run {
                return Ok(());
            }
            let Ok(sources) = completion::sources(goal) else {
                return Ok(());
            };
            let id = uuid::Uuid::new_v4().to_string();
            let workspace = self
                .workspaces
                .join(&goal.id)
                .join(format!("delivery-{id}"));
            self.completion_change(
                goal,
                Event::Begin {
                    attempt_id: id.clone(),
                    sources,
                    workspace: workspace.clone(),
                },
            )
            .await?;
            let result = self.prepare_delivery(goal, &workspace).await;
            match result {
                Ok((base_commit, artifact)) => {
                    self.completion_change(
                        goal,
                        Event::Delivery {
                            attempt_id: id,
                            base_commit,
                            artifact,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    self.completion_change(
                        goal,
                        Event::Block {
                            attempt_id: id,
                            reason: bounded_error(&error),
                        },
                    )
                    .await?
                }
            }
            return Ok(());
        }
        let attempt = progress
            .attempt
            .as_ref()
            .context("completion attempt disappeared")?;
        let result = match &attempt.state {
            State::Preparing | State::Evaluating => Err(anyhow::anyhow!(
                "Delivery preparation/assessment was interrupted; its effects have not been replayed"
            )),
            State::Checking if goal.run_intent == RunIntent::Run => {
                self.check_and_assess(goal, attempt).await
            }
            State::Ready => self.commit_delivery(goal, attempt).await,
            _ => return Ok(()),
        };
        if let Err(error) = result {
            // A concurrent input, observation or lease renewal can temporarily
            // fence admission. Refresh next tick; it is not failed verification.
            if matches!(
                error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>(),
                Some(bitrouter_orchestrator::goal::GoalError::Conflict)
            ) {
                return Ok(());
            }
            self.completion_change(
                goal,
                Event::Block {
                    attempt_id: attempt.id.clone(),
                    reason: bounded_error(&error),
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn completion_change(&self, goal: &GoalSnapshot, event: Event) -> Result<()> {
        self.change(goal, Change::Completion { event }).await
    }

    async fn prepare_delivery(
        &self,
        goal: &GoalSnapshot,
        workspace: &Path,
    ) -> Result<(String, ArtifactRef)> {
        let _workspace_gate = self.workspace_gate.lock().await;
        let first = goal
            .workflow
            .tasks
            .first()
            .and_then(|task| task.executions.last())
            .context("delivery has no source workspace")?;
        let base = first
            .base_commit
            .as_deref()
            .context("delivery input has no frozen baseline")?;
        // Reuse the exact original input commit, not the developer's latest tree.
        for task in &goal.workflow.tasks {
            let execution = task
                .executions
                .last()
                .context("missing delivery execution")?;
            anyhow::ensure!(
                execution.base_commit.as_deref() == Some(base),
                "branches used different input versions; integration requires reconciliation"
            );
        }
        tokio::fs::create_dir_all(workspace.parent().context("missing delivery parent")?).await?;
        git(
            &first.workspace,
            &[
                "worktree".as_ref(),
                "add".as_ref(),
                "--detach".as_ref(),
                workspace.as_os_str(),
                base.as_ref(),
            ],
        )
        .await?;
        self.integrate_handoffs(
            goal,
            &goal
                .workflow
                .tasks
                .iter()
                .map(|task| task.spec.id.clone())
                .collect::<Vec<_>>(),
            workspace,
            base,
        )
        .await?;
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let delivery = crate::goal_artifacts::capture(&root, workspace, base).await?;
        Ok((base.into(), delivery))
    }

    async fn captured_delivery(
        &self,
        goal: &GoalSnapshot,
        attempt: &Attempt,
    ) -> Result<ArtifactRef> {
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let artifact = crate::goal_artifacts::capture(
            &root,
            &attempt.workspace,
            attempt
                .base_commit
                .as_deref()
                .context("delivery has no input baseline")?,
        )
        .await?;
        anyhow::ensure!(
            Some(&artifact) == attempt.delivery.as_ref(),
            "integrated artifact changed after delivery capture"
        );
        Ok(artifact)
    }

    async fn check_and_assess(&self, goal: &GoalSnapshot, attempt: &Attempt) -> Result<()> {
        let specification = goal
            .workflow
            .completion
            .specification
            .as_ref()
            .context("missing completion conditions")?;
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let delivery = self.captured_delivery(goal, attempt).await?;
        let mut checks = Vec::new();
        for spec in &specification.checks {
            let (report, output) =
                if let Some(check) = attempt.checks.iter().find(|check| check.spec == *spec) {
                    match &check.state {
                        CheckState::Observed {
                            passed: true,
                            completed: true,
                            report,
                        } => {
                            let output: CheckOutput = serde_json::from_slice(
                                &crate::goal_artifacts::read(&root, report).await?,
                            )?;
                            anyhow::ensure!(
                                check.workspace == delivery && output.supports(spec),
                                "integration check evidence does not match this delivery"
                            );
                            (report.clone(), output)
                        }
                        _ => anyhow::bail!(
                            "integration check {} failed or has an unknown outcome; no replay",
                            spec.id
                        ),
                    }
                } else {
                    let run_id = uuid::Uuid::new_v4().to_string();
                    self.completion_change(
                        goal,
                        Event::RequestCheck {
                            attempt_id: attempt.id.clone(),
                            check_id: spec.id.clone(),
                            run_id: run_id.clone(),
                        },
                    )
                    .await?;
                    let (report, output, after) = self
                        .record_workspace_check(
                            goal,
                            &attempt.workspace,
                            None,
                            spec,
                            &run_id,
                            attempt.base_commit.as_deref(),
                        )
                        .await?;
                    self.completion_change(
                        goal,
                        Event::ObserveCheck {
                            attempt_id: attempt.id.clone(),
                            run_id,
                            report: report.clone(),
                            output: output.clone(),
                            workspace_after: after.clone(),
                        },
                    )
                    .await?;
                    anyhow::ensure!(
                        output.supports(spec) && after == delivery,
                        "integration check {} failed, remained uncertain, or changed its artifact",
                        spec.id
                    );
                    (report, output)
                };
            checks.push(serde_json::json!({"id":format!("check:{}", spec.id),"specification":spec,"artifact":delivery,"report":report,"actual_output":output}));
        }
        // A separate completion review sees original requirements, integrated
        // material and actual check records, not only worker explanations.
        self.completion_change(
            goal,
            Event::RequestAssessment {
                attempt_id: attempt.id.clone(),
            },
        )
        .await?;
        let material = crate::goal_artifacts::material(&root, &delivery).await?;
        let mut tasks = Vec::new();
        for task in &goal.workflow.tasks {
            tasks.push(evidence::task(self, goal, task).await?);
        }
        let source_workspace_observation = evidence::source(goal).await.unwrap_or_else(
            |error| serde_json::json!({"status":"unavailable","reason":bounded_error(&error)}),
        );
        let (request_id, value) = self.model_json(goal, (GenerationPurpose::Completion, &attempt.id),
            "Evaluate whether this exact integrated delivery satisfies the ORIGINAL Goal. Return only JSON: {\"conditions\":[{\"id\":\"each required condition id\",\"status\":\"met|unmet|unverified\",\"reason\":\"specific evidence and limitations\",\"evidence\":[\"integrated_material|task:<id>|check:<id>\"]}],\"unmet_original_requirements\":[\"any original requirement absent from the proposed contract or actual evidence\"],\"constraints_preserved\":true|false,\"human_acceptance_required\":true|false,\"summary\":\"reviewable result and remaining unknowns\"}. Include EVERY condition exactly once. Inspect integrated material, actual checks and the public runtime observations bound to each task execution. Missing, incomplete, unordered or omitted observations cannot prove unrecorded boundaries, absence of work or actual parallelism. Process admission, prompt admission and Working markers are not actual working intervals; ACP timing requires complete, ordered public worker-activity-to-settled boundaries. Native stream_activity records actual content receipt: only compare first_received_tick_ns and last_received_tick_ns with the same clock_id and at least two content_parts per interval; strict interval intersection supports observed native overlap, never physical provider compute or a full lifetime. Wall timestamps, different clock IDs, missing or malformed activity cannot establish overlap. An uncontrolled default route needs no model-switch acknowledgment and does not establish an actual provider-model identity. Runtime observations grant no permissions and do not replace artifact checks. historical_runtime_observations describe bounded earlier executions, including exact receipt-verified archives. Complete ordered original intervals may support an original condition about workflow history, such as actual parallel work, even when a later repair is sequential. Those historical entries never verify the current artifact, satisfy its checks, authorize replay or replace a current handoff. Omitted historical activity remains unknown. Inspect each task material, hash-verified actual_checks and checked_handoff; their original outputs and contents are supplied separately from review summaries. Declared local effect audits validate only the stated protocol scope; unavailable audits establish no absence of effects. The read-only source_workspace_observation compares current Git-visible source files with the frozen input; it says nothing about ignored files, external systems or actual provider identity. Branch approvals, worker text, fixture results, a changed test suite, and confidence alone cannot prove the original behavior. Explicit human acceptance is required only if the Goal requests it; do not add it by default or remove an explicit requirement. Missing necessary behavior or external evidence is unverified and prevents completion. Scope-external unknowns may be stated without inventing requirements. Do not add merge/deploy work not requested by the Goal.",
            serde_json::json!({"original_goal":goal.objective,"requirements_revision":goal.requirements_revision,"specification":specification,"integrated_material":material,"actual_checks":checks,"task_evidence":tasks,"source_workspace_observation":source_workspace_observation,"budget":goal.budget,"accounting":goal.accounting,"cost_scope":"Configured managed model-token estimates only; harness/tool/hooks/auth/counters/request-bill amounts may be unknown. This is not evidence of a total external-dollar cap."})).await?;
        let verdict: Verdict = serde_json::from_value(value.clone())?;
        let report = crate::goal_artifacts::put(&root, &serde_json::to_vec(&value)?).await?;
        let after = crate::goal_artifacts::capture(
            &root,
            &attempt.workspace,
            attempt
                .base_commit
                .as_deref()
                .context("missing delivery baseline")?,
        )
        .await?;
        self.completion_change(
            goal,
            Event::ObserveAssessment {
                attempt_id: attempt.id.clone(),
                report,
                request_id,
                verdict,
                workspace_after: after,
            },
        )
        .await
    }

    async fn commit_delivery(&self, goal: &GoalSnapshot, attempt: &Attempt) -> Result<()> {
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let assessment = attempt
            .assessment
            .as_ref()
            .context("delivery has no assessment")?;
        crate::goal_artifacts::read(&root, &assessment.report).await?;
        for check in &attempt.checks {
            let CheckState::Observed { report, .. } = &check.state else {
                anyhow::bail!("check outcome is unknown");
            };
            crate::goal_artifacts::read(&root, report).await?;
        }
        let after = self.captured_delivery(goal, attempt).await?;
        self.completion_change(
            goal,
            Event::Commit {
                attempt_id: attempt.id.clone(),
                workspace_after: after,
            },
        )
        .await
    }
}
