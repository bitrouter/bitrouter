//! Preserve source order around effects while allowing bounded read groups.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::tools;
use crate::core::protocol::ToolExecute;

#[derive(Default)]
pub(super) struct Tools {
    pending: VecDeque<ToolExecute>,
    running: BTreeMap<String, (String, bool)>,
    failed_steps: BTreeSet<String>,
}

impl Tools {
    pub(super) fn push(&mut self, dispatch: ToolExecute) {
        self.pending.push_back(dispatch);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn next(&mut self, parallel: usize) -> Option<(ToolExecute, bool)> {
        let mut blocked = BTreeSet::new();
        let index = self.pending.iter().position(|dispatch| {
            if !blocked.insert(dispatch.agent_id.clone()) {
                return false;
            }
            let running: Vec<_> = self
                .running
                .values()
                .filter(|(agent, _)| agent == &dispatch.agent_id)
                .collect();
            running.is_empty()
                || (tools::read_only(&dispatch.tool)
                    && running.len() < parallel.max(1)
                    && running.iter().all(|(_, read_only)| *read_only))
        })?;
        let dispatch = self.pending.remove(index)?;
        self.running.insert(
            dispatch.invocation_id.clone(),
            (dispatch.agent_id.clone(), tools::read_only(&dispatch.tool)),
        );
        let skip = self.failed_steps.contains(&dispatch.step_id);
        Some((dispatch, skip))
    }

    pub(super) fn finished(&mut self, dispatch: &ToolExecute, failed: bool) {
        self.running.remove(&dispatch.invocation_id);
        if failed {
            self.failed_steps.insert(dispatch.step_id.clone());
        }
    }
}
