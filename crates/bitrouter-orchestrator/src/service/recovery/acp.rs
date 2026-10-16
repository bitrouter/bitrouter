//! ACP continuation derives from the original public journal, never native context.
use crate::service::worker::acp::{Action, Binding, Continuation, Owner};
use crate::store::ExecutionRecord;

#[derive(serde::Serialize)]
pub(super) struct State {
    prompt: String,
    binding: Option<Binding>,
    owner: Option<Owner>,
    pending_start: bool,
    submitted: bool,
    observed: bool,
    pub(super) accepted_at_ms: Option<u64>,
}
impl State {
    pub(super) fn new(prompt: String) -> Self {
        Self {
            prompt,
            binding: None,
            owner: None,
            pending_start: false,
            submitted: false,
            observed: false,
            accepted_at_ms: None,
        }
    }
    pub(super) fn consume(&mut self, fact: &ExecutionRecord) -> Result<(), String> {
        match fact {
            ExecutionRecord::AcpDispatch {
                owner,
                action: Action::Start,
                ..
            } => {
                if self.pending_start {
                    return Err("unconfirmed ACP startup cannot be retried".into());
                }
                self.owner = Some(owner.clone());
                self.pending_start = true;
            }
            ExecutionRecord::AcpSession { binding } => {
                if !self.pending_start || self.owner.is_none() {
                    return Err("ACP session lacks its owned startup intent".into());
                }
                if binding.session_id.is_empty() || binding.run_id.is_empty() {
                    return Err("ACP session identity is empty".into());
                }
                if let Some(old) = &self.binding {
                    if binding.restored_from.as_ref() != Some(&old.run_id)
                        || binding.session_id != old.session_id
                        || binding.agent_id != old.agent_id
                        || self.submitted
                        || self.observed
                    {
                        return Err(
                            "ACP restored binding lacks original pre-prompt continuity".into()
                        );
                    }
                } else if binding.restored_from.is_some() {
                    return Err("ACP restored binding has no original session".into());
                }
                self.binding = Some(binding.as_ref().clone());
                self.pending_start = false;
            }
            ExecutionRecord::AcpDispatch {
                action: Action::Prompt,
                ..
            } => self.submitted = true,
            ExecutionRecord::AcpObservation { .. }
            | ExecutionRecord::AcpPermissionChoice { .. }
            | ExecutionRecord::AcpPermissionDispatch { .. } => self.observed = true,
            _ => {}
        }
        Ok(())
    }
    pub(super) fn continuation(&self) -> Option<Continuation> {
        if self.pending_start {
            return None;
        }
        let owner = self.owner.as_ref()?;
        let binding = self.binding.as_ref()?;
        let accepted_at_ms = self.accepted_at_ms.filter(|ms| *ms > 0)?;
        (!self.submitted && !self.observed && !self.prompt.trim().is_empty()).then(|| {
            Continuation {
                owner: owner.clone(),
                binding: binding.clone(),
                prompt: self.prompt.clone(),
                accepted_at_ms,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_restore_start_ack_never_reuses_the_previous_session_as_a_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = Owner {
            server_instance_id: "old".into(),
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        let mut state = State::new("original input".into());
        state.accepted_at_ms = Some(1);
        state.consume(&ExecutionRecord::AcpDispatch {
            owner: original.clone(),
            action: Action::Start,
            action_id: "start".into(),
        })?;
        state.consume(&ExecutionRecord::AcpSession {
            binding: Box::new(Binding {
                run_id: "original-run".into(),
                session_id: "original-session".into(),
                agent_id: "peer".into(),
                confirmed_model_route: None,
                capabilities: None,
                restored_from: None,
            }),
        })?;
        assert_eq!(
            state
                .continuation()
                .ok_or("original checkpoint missing")?
                .owner,
            original
        );
        let restored_owner = Owner {
            server_instance_id: "new".into(),
            ..original
        };
        let start = ExecutionRecord::AcpDispatch {
            owner: restored_owner.clone(),
            action: Action::Start,
            action_id: "restore".into(),
        };
        state.consume(&start)?;
        assert!(
            state.continuation().is_none(),
            "lost restore mapping reused an older run"
        );
        assert!(
            state.consume(&start).is_err(),
            "unknown startup was retried"
        );
        state.consume(&ExecutionRecord::AcpSession {
            binding: Box::new(Binding {
                run_id: "restored-run".into(),
                session_id: "original-session".into(),
                agent_id: "peer".into(),
                confirmed_model_route: None,
                capabilities: None,
                restored_from: Some("original-run".into()),
            }),
        })?;
        assert_eq!(
            state
                .continuation()
                .ok_or("confirmed restore missing")?
                .owner,
            restored_owner
        );
        state.consume(&ExecutionRecord::AcpDispatch {
            owner: restored_owner,
            action: Action::Prompt,
            action_id: "prompt".into(),
        })?;
        assert!(
            state.continuation().is_none(),
            "possibly submitted input was replayable"
        );
        Ok(())
    }
}
