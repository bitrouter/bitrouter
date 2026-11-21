//! Finalization shared by foreground and custom-listener hosts.

use std::sync::Arc;

use anyhow::Result;

use super::Assembled;
use crate::daemon::ObserveStatusProvider;

impl Assembled {
    /// Drain owned services and flush telemetry before dropping the async runtime.
    ///
    /// Stop accepting requests and await all application HTTP serving futures
    /// before calling this method. Keep the assembled value alive while serving;
    /// dropping it or cancelling this future cannot finish asynchronous cleanup.
    /// Durable outbox publication failures are logged and remain retryable on
    /// restart, as they are for the foreground host.
    ///
    /// # Errors
    /// Returns an error if tenant work cannot drain or receipt observation and
    /// its configured local exporter cannot complete cleanly. Telemetry cleanup
    /// is still attempted after a tenant drain failure.
    pub async fn shutdown(&self) -> Result<()> {
        self.shutdown_state().finish().await
    }

    pub(crate) fn shutdown_state(&self) -> Shutdown {
        Shutdown {
            tenancy: self.tenancy.clone(),
            observe: self.observe.clone(),
            #[cfg(feature = "eval-publication")]
            outbox: self.trajectory_outbox_publisher.clone(),
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            exporter: self.otel_exporter.clone(),
        }
    }
}

/// Keeps cleanup ownership after the foreground host distributes app handles.
pub(crate) struct Shutdown {
    tenancy: Option<crate::tenancy::Service>,
    observe: Arc<dyn ObserveStatusProvider>,
    #[cfg(feature = "eval-publication")]
    outbox: Option<crate::eval::publisher::TrajectoryOutboxPublisher>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    exporter: Option<Arc<crate::telemetry::exporter::OtelExporter>>,
}

impl Shutdown {
    pub(crate) async fn finish(self) -> Result<()> {
        let receipt_observer = self
            .tenancy
            .as_ref()
            .map(|service| service.receipt_observer())
            .unwrap_or_default();
        let tenancy_shutdown = match self.tenancy {
            Some(service) => service.shutdown().await,
            None => Ok(()),
        };

        #[cfg(feature = "eval-publication")]
        if let Some(publisher) = self.outbox {
            match publisher.drain_after_active_worker().await {
                Ok(summary) if summary.failed > 0 => tracing::warn!(
                    attempted = summary.attempted,
                    delivered = summary.delivered,
                    failed = summary.failed,
                    "trajectory outbox drain completed with poison items still pending"
                ),
                Ok(_) => {}
                Err(_) => tracing::warn!(
                    reason = "shutdown_drain_failed",
                    "trajectory outbox shutdown drain failed"
                ),
            }
        }

        // Seal observations after producers drain, then keep the runtime alive
        // while the exporter flushes on its blocking worker.
        let receipt_observation_clean = receipt_observer.seal(tenancy_shutdown.is_ok());
        self.observe.shutdown().await;
        let receipt_measurement = (|| -> Result<()> {
            anyhow::ensure!(
                receipt_observation_clean,
                "receipt latency observation did not seal cleanly"
            );
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            if receipt_observer.enabled() {
                anyhow::ensure!(
                    self.exporter
                        .as_ref()
                        .is_some_and(|exporter| exporter.recorded_shutdown_is_clean()),
                    "receipt latency local export did not shut down cleanly"
                );
            }
            Ok(())
        })();
        tenancy_shutdown.and(receipt_measurement)
    }
}
