//! One telemetry owner per process, borrowed by every routing generation.

use std::sync::Arc;

use anyhow::Result;
use tokio::time::Instant;

use crate::daemon::{NoopObserveStatus, ObserveStatusProvider};
use crate::telemetry::OTEL_ENABLED;
use crate::telemetry::config::OtelConfig;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
use crate::telemetry::exporter::OtelExporter;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
use crate::telemetry::owned::ProcessExporter;

pub(crate) enum Drain {
    Disabled,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    Enabled(crate::telemetry::owned::Drain),
}

impl Drain {
    pub fn is_quiescent(&self) -> bool {
        match self {
            Self::Disabled => true,
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            Self::Enabled(report) => report.quiescent,
        }
    }

    pub fn is_clean(&self) -> bool {
        match self {
            Self::Disabled => true,
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            Self::Enabled(report) => report.is_clean(),
        }
    }
}

pub(crate) struct Telemetry {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    owner: Option<ProcessExporter>,
    observe: Arc<dyn ObserveStatusProvider>,
}

impl Telemetry {
    pub fn new(config: Option<OtelConfig>) -> Result<Self> {
        #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
        anyhow::ensure!(
            config.is_none(),
            "shared telemetry requires an OTLP transport feature"
        );
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        let owner = config.map(ProcessExporter::new).transpose()?;
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        let observe: Arc<dyn ObserveStatusProvider> = match &owner {
            Some(owner) => Arc::new(Status(owner.exporter())),
            None => Arc::new(NoopObserveStatus {
                compiled_in: OTEL_ENABLED,
            }),
        };
        #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
        let observe = Arc::new(NoopObserveStatus {
            compiled_in: OTEL_ENABLED,
        });
        Ok(Self {
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            owner,
            observe,
        })
    }

    pub fn observe(&self) -> Arc<dyn ObserveStatusProvider> {
        self.observe.clone()
    }

    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub fn exporter(&self) -> Option<Arc<OtelExporter>> {
        self.owner.as_ref().map(ProcessExporter::exporter)
    }

    /// Only the process supervisor calls this after every generation has joined.
    pub async fn drain_until(&self, deadline: Instant) -> Result<Drain> {
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        if let Some(owner) = &self.owner {
            return Ok(Drain::Enabled(owner.drain_until(deadline).await?));
        }
        let _ = deadline;
        Ok(Drain::Disabled)
    }

    pub async fn force_join(&self) -> Result<Drain> {
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        if let Some(owner) = &self.owner {
            return Ok(Drain::Enabled(owner.force_join().await?));
        }
        Ok(Drain::Disabled)
    }
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
struct Status(Arc<OtelExporter>);

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
#[async_trait::async_trait]
impl ObserveStatusProvider for Status {
    fn status(&self) -> crate::daemon::ObserveStatusPayload {
        crate::assemble::otel_status(&self.0)
    }
    // The default shutdown is deliberately empty: a generation's status view
    // does not own the process exporter or its blocking/processor joins.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_process_telemetry_needs_no_runtime_resources() -> Result<()> {
        let telemetry = Telemetry::new(None)?;
        assert!(!telemetry.observe().status().exporter_wired);
        assert!(telemetry.drain_until(Instant::now()).await?.is_clean());
        assert!(telemetry.force_join().await?.is_quiescent());
        Ok(())
    }

    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    #[tokio::test]
    async fn generation_status_views_cannot_shutdown_the_process_exporter() -> Result<()> {
        let mut config = OtelConfig::default();
        config.metrics.enabled = false;
        let telemetry = Telemetry::new(Some(config))?;
        let first = telemetry.observe();
        let second = telemetry.observe();
        first.shutdown().await;
        drop(first);
        second.shutdown().await;
        let report = telemetry.force_join().await?;
        let Drain::Enabled(report) = report else {
            anyhow::bail!("configured exporter missing");
        };
        anyhow::ensure!(report.quiescent && report.workers.cancelled == 1);
        Ok(())
    }

    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    #[tokio::test]
    #[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
    async fn separate_assembled_generations_borrow_one_process_exporter() -> Result<()> {
        use crate::gateway::shared::continuation::SharedContinuationDb;
        use anyhow::Context as _;
        use std::time::Duration;

        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
        let db = super::super::store::connect(&url, 4, Duration::from_secs(3)).await?;
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            crate::db::run_migrations(&db).await?;
            let capability = SharedContinuationDb::fixture(db.clone()).await?;
            let peer = capability.fixture_peer().await?;
            let mut otel = OtelConfig::default();
            otel.metrics.enabled = false;
            let telemetry = Arc::new(Telemetry::new(Some(otel))?);
            let exporter = telemetry.exporter().context("process exporter missing")?;
            let mut config = crate::config::parse_with(
                r#"
inherit_defaults: false
registry:
  enabled: false
database:
  url: postgres://127.0.0.1/not-opened
gateway_state:
  mode: shared
  namespace: assembly-telemetry
  deployment_key_env: UNUSED_ASSEMBLY_TEST_KEY
plugins:
  bitrouter-telemetry:
    otel:
      endpoint: "http://[invalid-candidate-endpoint"
"#,
                |_| None,
            )?;
            // The process input is authoritative. In particular, building a
            // generation must not attempt another local exporter constructor.
            let first_inputs = crate::assemble::SharedAssembly {
                continuation: capability,
                telemetry: telemetry.clone(),
                auth_appliers: crate::gateway::providers::auth::AuthAppliers::new(),
                correlation_key: crate::trajectory::canonical::CorrelationKey::from_bytes(
                    rand::random(),
                )?,
                policies: Vec::new(),
                policy_lock: None,
            };
            let second_inputs = crate::assemble::SharedAssembly {
                continuation: peer,
                telemetry: telemetry.clone(),
                auth_appliers: first_inputs.auth_appliers.clone(),
                correlation_key: first_inputs.correlation_key.clone(),
                policies: Vec::new(),
                policy_lock: None,
            };
            let mismatch = crate::assemble::build_shared_app(&config, &first_inputs)
                .await
                .err()
                .context("assembly accepted a different authority namespace")?;
            anyhow::ensure!(
                mismatch.to_string()
                    == "shared assembly namespace differs from its injected authority"
            );
            let crate::config::gateway_state::GatewayStateConfig::Shared(shared) =
                &mut config.gateway_state
            else {
                anyhow::bail!("shared fixture configuration missing");
            };
            shared.namespace = first_inputs.continuation.namespace().into();
            let first = crate::assemble::build_shared_app(&config, &first_inputs).await?;
            let second = crate::assemble::build_shared_app(&config, &second_inputs).await?;
            for assembled in [&first, &second] {
                anyhow::ensure!(Arc::ptr_eq(
                    assembled
                        .otel_exporter
                        .as_ref()
                        .context("generation exporter missing")?,
                    &exporter
                ));
                let tasks = assembled
                    .app
                    .language_model()
                    .context("pipeline missing")?
                    .owned_tasks()
                    .context("shared pipeline owner missing")?;
                tasks.close_sources();
                anyhow::ensure!(matches!(
                    tasks
                        .drain_until(Instant::now() + Duration::from_secs(5))
                        .await?,
                    crate::gateway::pipeline::owned_tasks::Drain::Joined(_)
                ));
                anyhow::ensure!(
                    assembled
                        .continuation_registry
                        .drain_owned_until(Instant::now() + Duration::from_secs(5))
                        .await?
                        .is_clean()
                );
                assembled.observe.shutdown().await;
            }
            drop((first, second, first_inputs, second_inputs));
            let Drain::Enabled(report) = telemetry.force_join().await? else {
                anyhow::bail!("process exporter missing at shutdown");
            };
            anyhow::ensure!(report.quiescent && report.workers.cancelled == 1);
            Ok::<(), anyhow::Error>(())
        })
        .await;
        let closed = db.close().await;
        result??;
        closed?;
        Ok(())
    }
}
