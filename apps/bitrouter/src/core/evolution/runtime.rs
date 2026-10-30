//! Authenticated canonical admission and actual model execution bookkeeping.

use std::sync::{Arc, Mutex};

use crate::config::routing_table::ConfigRoutingTable;
use anyhow::{Context, Result, ensure};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::gateway::routing::blocks::catalog::block_digest;
use crate::gateway::routing::blocks::control::{BlockDefinition, ControlState};
use crate::gateway::routing::blocks::store::BlockPolicyStore;
use crate::policy_lock::PolicyRuntime;
use crate::storage::evolution::EvolutionStore;
use crate::storage::session_identity::SessionIdentity;

use crate::storage::execution::{EXECUTION_KIND, ExecutionRecord};

pub mod candidates;

#[derive(Clone)]
pub struct EvolutionRuntime {
    pub(crate) db: DatabaseConnection,
    routing: Arc<ConfigRoutingTable>,
    policies: Arc<PolicyRuntime>,
    pub(crate) worker_status: Arc<Mutex<super::scheduler::WorkerStatus>>,
}

impl EvolutionRuntime {
    pub fn new(
        db: DatabaseConnection,
        routing: Arc<ConfigRoutingTable>,
        policies: Arc<PolicyRuntime>,
    ) -> Self {
        Self {
            worker_status: Arc::new(Mutex::new(Default::default())),
            db,
            routing,
            policies,
        }
    }

    pub fn worker_status(&self) -> super::scheduler::WorkerStatus {
        match self.worker_status.lock() {
            Ok(status) => status.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Background publication validates the same live route dependencies as admission.
    pub async fn reconcile(
        &self,
        owner: &str,
        block_id: &str,
    ) -> Result<super::learning::LearningReport> {
        self.reconcile_experiment(owner, block_id, None).await
    }

    pub async fn reconcile_experiment(
        &self,
        owner: &str,
        block_id: &str,
        experiment_id: Option<&str>,
    ) -> Result<super::learning::LearningReport> {
        let service = self.service(owner)?;
        let state = service.state().await?;
        let block = state
            .experiment(block_id, experiment_id)
            .context("unknown experiment")?;
        let (generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let dependency = block_digest(&config, &policies, &block.definition).await?;
        ensure!(
            dependency == block.routing_config_digest,
            "routing dependency changed; register a new experiment revision"
        );
        service
            .reconcile_checked(block_id, Some(&block.experiment_id), || {
                ensure!(
                    self.routing.generation() == generation
                        && self.policies.matches_routing_snapshot(&policies),
                    "routing reloaded before evolution publication; retry"
                );
                Ok(())
            })
            .await
    }

    pub fn service(&self, owner: &str) -> Result<BlockPolicyStore> {
        BlockPolicyStore::new(self.db.clone(), owner)
    }

    /// Operator-owned definitions are validated against the live route catalog.
    /// This registers an experiment but never enables the owner's mode.
    pub async fn register(&self, owner: &str, definition: BlockDefinition) -> Result<ControlState> {
        let (generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let dependency = block_digest(&config, &policies, &definition).await?;
        ensure!(
            generation == self.routing.generation()
                && self.policies.matches_routing_snapshot(&policies),
            "routing changed during block validation; retry registration"
        );
        self.service(owner)?
            .register_checked(definition, dependency, None, || {
                ensure!(
                    generation == self.routing.generation()
                        && self.policies.matches_routing_snapshot(&policies),
                    "routing changed before block registration; retry"
                );
                Ok(())
            })
            .await
    }

    /// Start a new experiment while retaining old assignments and feedback.
    /// A changed live contract rebases to the configured selector; callers must
    /// explicitly supply those baseline routes in the reviewed definition.
    pub async fn revise(
        &self,
        owner: &str,
        definition: BlockDefinition,
        predecessor: String,
    ) -> Result<ControlState> {
        let service = self.service(owner)?;
        let state = service.state().await?;
        let previous = state
            .experiment(&definition.block_id, Some(&predecessor))
            .context("unknown predecessor experiment")?;
        let (generation, config) = self.routing.versioned_snapshot();
        let policies = self.policies.routing_snapshot();
        let dependency = block_digest(&config, &policies, &definition).await?;
        let previous_dependency = block_digest(&config, &policies, &previous.definition).await;
        let reset_to_configured = previous_dependency.as_ref().ok()
            != Some(&previous.routing_config_digest)
            || !state.external_dependencies_match(previous);
        service
            .revise_checked(
                super::service::revisions::RevisionRegistration {
                    definition,
                    routing_digest: dependency,
                    predecessor,
                    reset_to_configured,
                    expected_control: (state.generation, state.mode_epoch),
                },
                || {
                    ensure!(
                        self.routing.generation() == generation
                            && self.policies.matches_routing_snapshot(&policies),
                        "routing changed before experiment revision; retry"
                    );
                    Ok(())
                },
            )
            .await
    }

    pub async fn executions(&self, identity: &SessionIdentity) -> Result<Vec<ExecutionRecord>> {
        let store = EvolutionStore::new(self.db.clone(), &identity.owner)?;
        let key = identity.key()?;
        let mut rows: Vec<ExecutionRecord> = crate::storage::evolution::records::Entity::find()
            .filter(crate::storage::legacy::scope("checkpoint_evolution"))
            .filter(crate::storage::evolution::records::Column::ScopeId.eq(&store.scope_id))
            .filter(crate::storage::evolution::records::Column::Kind.eq(EXECUTION_KIND))
            .filter(crate::storage::evolution::records::Column::SessionKey.eq(key))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| serde_json::from_str(&row.body))
            .collect::<std::result::Result<_, _>>()?;
        rows.sort_by(|a, b| (&a.started_at, &a.request_id).cmp(&(&b.started_at, &b.request_id)));
        Ok(rows)
    }
}

#[cfg(test)]
mod tests;
