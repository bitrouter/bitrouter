use bitrouter_config::BitrouterConfig;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct ServerPlan {
    pub config: BitrouterConfig,
}

impl ServerPlan {
    pub fn new(config: BitrouterConfig) -> Self {
        Self { config }
    }

    pub async fn serve(self) -> Result<()> {
        tracing::info!(listen = %self.config.server.listen, "runtime server scaffold is not wired yet");
        Ok(())
    }
}
