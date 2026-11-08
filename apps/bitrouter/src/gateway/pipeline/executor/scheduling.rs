//! Mandatory scheduler evidence, separate from best-effort observe hooks.

use super::{PipelineContext, Result, RoutingTarget, StreamPart};

#[derive(Clone, Default)]
pub(super) struct Attempt {
    #[cfg(feature = "inference-routing")]
    observer: Option<crate::gateway::scheduling::Observation>,
}

impl Attempt {
    pub(super) fn from_context(ctx: &PipelineContext) -> Self {
        let _ = ctx;
        Self {
            #[cfg(feature = "inference-routing")]
            observer: ctx.scheduling_attempt(),
        }
    }
    pub(super) fn active(&self) -> bool {
        #[cfg(feature = "inference-routing")]
        {
            self.observer.is_some()
        }
        #[cfg(not(feature = "inference-routing"))]
        {
            false
        }
    }
    pub(super) fn begin_send(&self, target: &RoutingTarget) -> Result<()> {
        let _ = target;
        #[cfg(feature = "inference-routing")]
        if let Some(observer) = &self.observer {
            observer.begin_send(target)?;
        }
        Ok(())
    }
    pub(super) fn headers(&self, status: u16) {
        let _ = status;
        #[cfg(feature = "inference-routing")]
        if let Some(observer) = &self.observer {
            observer.headers(status);
        }
    }
    pub(super) fn parts(&self, parts: &[StreamPart]) {
        let _ = parts;
        #[cfg(feature = "inference-routing")]
        if let Some(observer) = &self.observer {
            for part in parts {
                observer.part(part);
            }
        }
    }
    pub(super) fn completed(&self, success: bool, usage: Option<&bitrouter_ai::types::Usage>) {
        let _ = (success, usage);
        #[cfg(feature = "inference-routing")]
        if let Some(observer) = &self.observer {
            if let Some(usage) = usage {
                observer.usage(usage);
            }
            observer.completed(success);
        }
    }
}
