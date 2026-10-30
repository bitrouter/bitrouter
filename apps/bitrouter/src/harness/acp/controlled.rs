//! Local authenticated controller hosts over the existing ACP client/factory.
//! No tenant session HTTP API, background paid judge, or raw-history export.

pub(crate) mod factory;
#[cfg(feature = "core")]
mod owned;

#[cfg(feature = "core")]
use anyhow::Result;
#[cfg(feature = "core")]
use std::{path::PathBuf, sync::Arc};
#[cfg(feature = "core")]
use tokio_util::sync::CancellationToken;

/// Trusted embedding launch data, not tenant configuration or live authority.
/// The environment must be explicit; inherited process variables are cleared.
#[cfg(feature = "core")]
pub struct LocalControllerLaunch {
    /// Label for the host's configured ACP executable.
    pub source: String,
    /// Existing trusted command/args/environment descriptor.
    pub agent: crate::config::agent::AcpAgentConfig,
    /// Loopback origin of the already-running listener serving the same App.
    pub listener_origin: String,
    /// Existing process/client options. Terminal login is unsupported here.
    pub options: crate::acp_cli::LaunchOptions,
}

/// Bounded reference to a stopped prefix, never the captured transcript.
#[cfg(feature = "core")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointSummary {
    /// Immutable owned checkpoint identity.
    pub checkpoint_id: String,
    /// Exact observed session watermark.
    pub watermark: i64,
    /// Digest of immutable namespaced observation references.
    pub prefix_digest: String,
    /// Number of observation references, including session setup.
    pub observations: usize,
}

/// Authorized bounded review of one immutable owned checkpoint.
/// This projection is an in-process value, not a wire or authority contract.
#[cfg(feature = "core")]
#[derive(Debug)]
pub struct CheckpointReview {
    /// Exact immutable checkpoint referenced by the evidence.
    pub checkpoint: CheckpointSummary,
    /// Fixed rubric, cited evidence and exact expected effective revision.
    pub scoring: crate::core::evolution::scoring::ScoringInput,
    /// Supported prefill rubric, absent for a Human retraction.
    pub previous: Option<crate::core::evolution::rubric::RubricEvaluation>,
    /// The immutable revision supplying the prefill, including tombstones.
    pub prefill_revision: Option<String>,
    /// Complete bounded history of this checkpoint in deterministic order.
    pub history: Vec<crate::acp_trajectory::checkpoint::types::AssessmentRevision>,
    /// Current validated session watermark in the same snapshot.
    pub current_watermark: i64,
}

/// One Service-owned controller. Dropping the handle requests stop; its owner
/// continues through child reaping and pending capture transaction settlement.
#[cfg(feature = "core")]
pub struct ControlledSession {
    handle: owned::Handle,
}

#[cfg(feature = "core")]
impl ControlledSession {
    #[cfg(all(test, unix))]
    pub(crate) fn ingress_statistics(&self) -> Arc<super::up::owned::ingress::Statistics> {
        self.handle.ingress.clone()
    }
    /// Authenticate against this App before capture I/O or child creation.
    /// Requires tenant-wide ExecutionSubmit, EvaluationSubmit, ResourceRead and
    /// CredentialUse, one retained route, and a provider-configurable ACP agent.
    pub async fn open_authenticated(
        app: Arc<crate::gateway::assembly::App>,
        headers: http::HeaderMap,
        launch: LocalControllerLaunch,
        cancel: CancellationToken,
    ) -> Result<Self> {
        owned::open(app, headers, launch, cancel).await
    }
    /// Create a fresh native session and confirm its model is the fixed route.
    /// This profile requires an advertised select option with ID `model`.
    pub async fn new_session(&self, cwd: PathBuf) -> Result<String> {
        self.handle.new_session(cwd).await
    }
    /// Execute a bounded prompt through the existing ACP client. Concurrent
    /// operations fail capacity admission; no unbounded client queue is exposed.
    pub async fn prompt(&self, native: &str, text: &str) -> Result<()> {
        self.handle.prompt(native, text).await
    }
    /// Freeze a stopped prefix in Observe/passive Explore. Off returns None.
    pub async fn checkpoint(&self, native: &str) -> Result<Option<CheckpointSummary>> {
        self.handle.checkpoint(native).await
    }
    /// Review an already committed checkpoint without starting an evaluator.
    /// `None` selects this association's latest committed stopped prefix.
    ///
    /// # Errors
    /// Rejects foreign/missing checkpoints, changed authority, closing hosts,
    /// corrupt retained evidence and bounded capacity exhaustion.
    pub async fn review_checkpoint(
        &self,
        native: &str,
        checkpoint_id: Option<&str>,
    ) -> Result<CheckpointReview> {
        self.handle.review_checkpoint(native, checkpoint_id).await
    }
    /// Submit an explicit Human correction with the reviewed expected head.
    /// Evaluator labels are normalized to the authenticated stable principal.
    ///
    /// # Errors
    /// Rejects invalid rubric input, stale heads, changed authority, unknown
    /// prior transaction custody and history capacity reserved for paid work.
    pub async fn submit_assessment(
        &self,
        native: &str,
        submission: crate::core::evolution::scoring::RubricSubmission,
    ) -> Result<crate::core::evolution::scoring::ScoringReceipt> {
        self.handle.submit_assessment(native, submission).await
    }
    /// Retract the current same-checkpoint assessment with a Human tombstone.
    /// Input must have Human source, no assessment, and an explicit reason.
    ///
    /// # Errors
    /// Rejects invalid retractions, stale heads, changed authority, closing
    /// hosts, unknown transaction custody and bounded capacity exhaustion.
    pub async fn retract_assessment(
        &self,
        native: &str,
        input: crate::acp_trajectory::checkpoint::types::RevisionInput,
    ) -> Result<crate::acp_trajectory::checkpoint::types::AssessmentRevision> {
        self.handle.retract_assessment(native, input).await
    }
    /// Bounded, lossy live rendering updates. Durable capture uses the proxy,
    /// independently of this UI stream. Subscribe before opening a native session.
    pub fn subscribe_updates(
        &self,
    ) -> std::pin::Pin<
        Box<dyn futures::Stream<Item = crate::harness::acp::translate::SessionUpdateKind> + Send>,
    > {
        self.handle.updates()
    }
    /// Existing single-consumer permission stream; approvals cross the private
    /// live-authority gate again before the controller delivers them.
    pub fn subscribe_permissions(
        &self,
    ) -> std::pin::Pin<
        Box<dyn futures::Stream<Item = crate::harness::acp::client::PendingPermission> + Send>,
    > {
        self.handle.permissions()
    }
    /// Request teardown and await the same retained owner's completion proof.
    pub async fn shutdown(self) -> Result<()> {
        self.handle.shutdown().await
    }
}
#[cfg(feature = "core")]
impl Drop for ControlledSession {
    fn drop(&mut self) {
        self.handle.stop();
    }
}

/// Actual cleanup evidence, constructible only inside this controller module.
/// It has no public constructor, persisted-data constructor or scope setter.
#[cfg(feature = "core")]
pub(crate) struct CleanupProof {
    binding: Arc<crate::tenancy::capture::Binding>,
}
#[cfg(feature = "core")]
impl CleanupProof {
    pub(crate) fn binding(&self) -> &Arc<crate::tenancy::capture::Binding> {
        &self.binding
    }
}
