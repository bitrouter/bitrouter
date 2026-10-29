//! Zero-work local bridge to the optional shared dispatch authority.

use super::{AppliedAuth, PipelineContext, Result, RoutingTarget};
use bitrouter_ai::types::{StreamPart, Usage};

pub(super) fn has_admission(ctx: &PipelineContext) -> bool {
    #[cfg(feature = "multi-instance")]
    {
        ctx.extension::<crate::gateway::shared::admitted::SharedAdmission>()
            .is_some()
    }
    #[cfg(not(feature = "multi-instance"))]
    {
        let _ = ctx;
        false
    }
}

pub(super) struct Attempt {
    #[cfg(feature = "multi-instance")]
    shared: Option<crate::gateway::shared::dispatched::Attempt>,
}

impl Attempt {
    pub fn is_shared(&self) -> bool {
        #[cfg(feature = "multi-instance")]
        {
            self.shared.is_some()
        }
        #[cfg(not(feature = "multi-instance"))]
        {
            false
        }
    }

    pub async fn begin(
        required: bool,
        applied: &AppliedAuth,
        target: &RoutingTarget,
        ctx: &PipelineContext,
    ) -> Result<Self> {
        #[cfg(feature = "multi-instance")]
        {
            use crate::gateway::error::BitrouterError;
            use crate::gateway::shared::{admitted::SharedAdmission, dispatched};
            let admission = ctx.extension::<SharedAdmission>();
            let stamp = applied.shared_credential()?;
            let shared = match (admission, stamp) {
                (Some(admission), Some(stamp)) => {
                    Some(dispatched::Attempt::begin(admission, &stamp, target, ctx).await?)
                }
                (None, None) if !required => None,
                _ => return Err(BitrouterError::UpstreamUnavailable),
            };
            Ok(Self { shared })
        }
        #[cfg(not(feature = "multi-instance"))]
        {
            let _ = (required, applied, target, ctx);
            Ok(Self {})
        }
    }

    pub async fn rejected(&mut self, status: u16) -> Result<()> {
        #[cfg(feature = "multi-instance")]
        if let Some(shared) = &mut self.shared {
            return shared.rejected(status).await;
        }
        let _ = status;
        Ok(())
    }

    pub async fn complete(&mut self, successful: bool, usage: Option<&Usage>) -> Result<()> {
        #[cfg(feature = "multi-instance")]
        if let Some(shared) = &mut self.shared {
            let outcome = if successful {
                bitrouter_ai::gateway::execution::TerminalOutcome::Succeeded
            } else {
                crate::gateway::shared::dispatched::failed("provider_failed")
            };
            return shared.complete(outcome, usage).await;
        }
        let _ = (successful, usage);
        Ok(())
    }

    pub fn observe(&mut self, part: &StreamPart) {
        #[cfg(feature = "multi-instance")]
        if let Some(shared) = &mut self.shared {
            shared.observe(part);
        }
        let _ = part;
    }

    /// Capture a complete decoded event before yielding any of its parts. A
    /// client can stop polling between usage/content and the terminal in the
    /// same event, but that cannot erase provider evidence already received.
    pub fn observe_batch(&mut self, parts: &[StreamPart], mut terminal: bool) -> Result<()> {
        if !self.is_shared() {
            return Ok(());
        }
        for part in parts {
            if terminal && !matches!(part, StreamPart::Usage { .. }) {
                return Err(
                    crate::gateway::error::BitrouterError::UpstreamInvalidResponse {
                        message: "upstream sent content after its terminal".into(),
                        usage: None,
                    },
                );
            }
            terminal |= part.is_terminal();
        }
        for part in parts {
            self.observe(part);
        }
        Ok(())
    }

    pub async fn finish_stream(&mut self) -> Result<()> {
        #[cfg(feature = "multi-instance")]
        if let Some(shared) = &mut self.shared {
            return shared.finish_stream().await;
        }
        Ok(())
    }

    pub async fn failed_stream(&mut self) -> Result<()> {
        #[cfg(feature = "multi-instance")]
        if let Some(shared) = &mut self.shared {
            return shared.failed_stream().await;
        }
        Ok(())
    }
}
