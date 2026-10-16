//! Goal ownership and interrupted admission links over the shared ThreadService.

use super::*;
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::thread::{
    PermissionProfile, ThreadRequest, ThreadSnapshot, ThreadTarget,
};

impl Coordinator {
    pub(super) async fn owner_thread(&self, goal: &GoalSnapshot) -> Result<ThreadSnapshot> {
        let epoch = self.threads.capabilities().server_instance_id;
        let caller = CallerContext::local();
        let workspace = goal.workspace.clone();
        let thread = if let Some(thread_id) = &goal.workflow.owner_thread_id {
            self.threads
                .read_stored_thread_view(
                    &ThreadTarget {
                        thread_id: thread_id.clone(),
                        server_instance_id: epoch,
                    },
                    &caller,
                )
                .await?
                .thread
        } else {
            // The Goal ID was saved before this action. Its deterministic key
            // survives a crash after Thread acceptance but before Goal binding.
            let key = bitrouter_orchestrator::goal::workflow::owner_thread_key(&goal.id);
            let thread = match self
                .threads
                .find_thread_admission(&epoch, &caller, &key)
                .await?
            {
                Some(thread) => thread,
                None => {
                    let config = self.routing.snapshot_config();
                    let model = crate::policy_lock::native_default_model(&config)
                        .context("no reasoning route is available")?;
                    let mut agent = AgentConfig::fixed(model, None)
                        .with_model_mode(crate::policy_lock::native_model_mode(&config, model)?)
                        .read_only();
                    agent.max_context_bytes =
                        self.threads.capabilities().limits.context_bytes_per_thread;
                    self.threads
                        .create_goal_owner_thread(
                            &epoch,
                            &goal.id,
                            ThreadRequest {
                                caller,
                                workspace,
                                config: agent,
                                permission_profile: PermissionProfile::ReadOnly,
                                verification_command: None,
                                idempotency_key: key.clone(),
                            },
                        )
                        .await?
                }
            };
            anyhow::ensure!(
                thread.owned_goal_id.as_deref() == Some(goal.id.as_str()),
                "owner admission key does not identify this Goal"
            );
            self.goals
                .workflow_change(
                    WorkflowActor {
                        owner: "local",
                        coordinator_id: &self.id,
                    },
                    &goal.id,
                    &format!("{key}-bind"),
                    goal.requirements_revision,
                    Change::BindOwnerThread {
                        thread_id: thread.thread_id.clone(),
                    },
                    now()?,
                )
                .await?;
            thread
        };
        anyhow::ensure!(
            thread.owned_goal_id.as_deref() == Some(goal.id.as_str()),
            "stored owner Thread does not own this Goal"
        );
        Ok(thread)
    }

    pub(super) async fn input_thread(
        &self,
        goal: &GoalSnapshot,
        owner: &ThreadSnapshot,
    ) -> Result<ThreadSnapshot> {
        let epoch = self.threads.capabilities().server_instance_id;
        let caller = CallerContext::local();
        let owner_target = ThreadTarget {
            thread_id: owner.thread_id.clone(),
            server_instance_id: epoch.clone(),
        };
        let thread = if let Some(id) = &goal.workflow.input_thread_id {
            self.threads
                .read_stored_thread_view(
                    &ThreadTarget {
                        thread_id: id.clone(),
                        server_instance_id: epoch,
                    },
                    &caller,
                )
                .await?
                .thread
        } else {
            let key = bitrouter_orchestrator::goal::workflow::input_thread_key(&goal.id);
            let thread = match self
                .threads
                .find_thread_admission(&epoch, &caller, &key)
                .await?
            {
                Some(thread) => thread,
                None => {
                    let owner_view = self
                        .threads
                        .read_stored_thread_view(&owner_target, &caller)
                        .await?;
                    self.threads
                        .create_goal_input_thread(
                            &owner_target,
                            ThreadRequest {
                                caller,
                                workspace: goal.workspace.clone(),
                                config: owner_view.config,
                                permission_profile: PermissionProfile::ReadOnly,
                                verification_command: None,
                                idempotency_key: key.clone(),
                            },
                        )
                        .await?
                }
            };
            anyhow::ensure!(
                thread.input_for_thread_id.as_ref() == Some(&owner.thread_id),
                "input admission does not belong to this Goal owner"
            );
            self.goals
                .workflow_change(
                    WorkflowActor {
                        owner: "local",
                        coordinator_id: &self.id,
                    },
                    &goal.id,
                    &format!("{key}-bind"),
                    goal.requirements_revision,
                    Change::BindInputThread {
                        thread_id: thread.thread_id.clone(),
                    },
                    now()?,
                )
                .await?;
            thread
        };
        anyhow::ensure!(
            thread.input_for_thread_id.as_ref() == Some(&owner.thread_id)
                && thread.owned_goal_id.is_none(),
            "stored input Thread has the wrong Goal parent"
        );
        Ok(thread)
    }
}
