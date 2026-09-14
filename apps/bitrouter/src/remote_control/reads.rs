//! One disclosure boundary for scoped HTTP control inspection.

use std::path::PathBuf;

use crate::actions::models::ModelsReport;
use crate::actions::route::{RouteInput, RouteReport};
use crate::actions::status::StatusReport;

use crate::paths::ConfigSource;

#[derive(Clone)]
pub(super) struct ReadPorts {
    pub source: ConfigSource,
    pub socket: PathBuf,
}

impl ReadPorts {
    pub async fn status_report(&self) -> anyhow::Result<StatusReport> {
        let mut report =
            crate::actions::status::DaemonStatus::new(&self.socket, Some(self.source.clone()))
                .report()
                .await
                .map_err(|_| anyhow::anyhow!("status_unavailable"))?;
        report.socket = None;
        Ok(report)
    }

    pub async fn models_report(&self) -> anyhow::Result<ModelsReport> {
        crate::actions::models::RoutableModels::new(self.source.clone(), Some(self.socket.clone()))
            .report()
            .await
            .map_err(|_| anyhow::anyhow!("models_unavailable"))
    }

    pub async fn route_report(&self, input: RouteInput) -> anyhow::Result<RouteReport> {
        crate::actions::administration::validate_identifier(&input.model)?;
        crate::actions::route::RouteAction::new(self.source.clone(), Some(self.socket.clone()))
            .report(input)
            .await
            .map_err(|_| anyhow::anyhow!("route_unavailable"))
    }
}
