use crate::{error::Result, paths::RuntimePaths};

#[derive(Debug, Clone)]
pub struct ControlClient {
    paths: RuntimePaths,
}

impl ControlClient {
    pub fn new(paths: RuntimePaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    pub async fn ping(&self) -> Result<()> {
        Ok(())
    }
}
