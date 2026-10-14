//! Client admission lanes. Uncertain input never blocks another Goal or Stop.
use super::*;
use std::collections::{BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Scope {
    New,
    Input(String),
    Control(String),
    Decision(String, String),
    Acceptance(String, String),
    Budget(String),
}

impl Scope {
    fn lane(&self) -> u8 {
        match self {
            Self::New | Self::Input(_) => 0,
            Self::Control(_) => 1,
            _ => 2,
        }
    }
    pub(super) fn effect(effect: &Effect) -> Option<Self> {
        match effect {
            Effect::Create { .. } => Some(Self::New),
            Effect::Submit { goal_id, .. } => Some(Self::Input(goal_id.clone())),
            Effect::Control { goal_id, .. } => Some(Self::Control(goal_id.clone())),
            Effect::Respond {
                goal_id,
                decision_id,
                ..
            } => Some(Self::Decision(goal_id.clone(), decision_id.clone())),
            Effect::Accept {
                goal_id,
                attempt_id,
                ..
            } => Some(Self::Acceptance(goal_id.clone(), attempt_id.clone())),
            Effect::SetBudget { goal_id, .. } => Some(Self::Budget(goal_id.clone())),
            _ => None,
        }
    }
    fn pending(pending: &Pending) -> Result<Self> {
        match &pending.command {
            GoalCommand::Create { .. } => Ok(Self::New),
            GoalCommand::Input { goal_id, .. } => Ok(Self::Input(goal_id.clone())),
            GoalCommand::Control { goal_id, .. } => Ok(Self::Control(goal_id.clone())),
            GoalCommand::RespondDecision {
                goal_id,
                decision_id,
                ..
            } => Ok(Self::Decision(goal_id.clone(), decision_id.clone())),
            GoalCommand::AcceptDelivery {
                goal_id,
                attempt_id,
                ..
            } => Ok(Self::Acceptance(goal_id.clone(), attempt_id.clone())),
            GoalCommand::SetBudget { goal_id, .. } => Ok(Self::Budget(goal_id.clone())),
            _ => anyhow::bail!("client admission must be a mutation"),
        }
    }
}

#[derive(Default)]
pub(super) struct Submissions {
    active: BTreeSet<Scope>,
    unresolved: VecDeque<Pending>,
}

impl Submissions {
    fn has_capacity(&self, scope: &Scope) -> bool {
        // Keep explicit control available even when eight input lookups stall.
        self.active
            .iter()
            .filter(|active| active.lane() == scope.lane())
            .count()
            < 8
    }
    pub(super) fn begin(&mut self, effect: &Effect) -> bool {
        let Some(scope) = Scope::effect(effect) else {
            return false;
        };
        if !self.has_capacity(&scope) || self.blocked(&scope) {
            return false;
        }
        self.active.insert(scope)
    }
    pub(super) fn blocked(&self, scope: &Scope) -> bool {
        self.active.contains(scope)
            || self.unresolved.iter().any(|pending| {
                Scope::pending(pending)
                    .as_ref()
                    .is_ok_and(|value| value == scope)
            })
    }
    pub(super) fn finish_effect(&mut self, effect: &Effect) {
        if let Some(scope) = Scope::effect(effect) {
            self.active.remove(&scope);
        }
    }
    pub(super) fn finish(&mut self, pending: &Pending) -> Result<()> {
        self.active.remove(&Scope::pending(pending)?);
        Ok(())
    }
    pub(super) fn retain(&mut self, pending: Pending) -> Result<()> {
        self.finish(&pending)?;
        self.unresolved.push_back(pending);
        Ok(())
    }
    pub(super) fn next(&mut self) -> Result<Option<Pending>> {
        for _ in 0..self.unresolved.len() {
            let Some(pending) = self.unresolved.pop_front() else {
                return Ok(None);
            };
            let scope = Scope::pending(&pending)?;
            if self.has_capacity(&scope) && self.active.insert(scope) {
                return Ok(Some(pending));
            }
            self.unresolved.push_back(pending);
        }
        Ok(None)
    }
    pub(super) fn sync(&self, state: &mut State) {
        state.new_pending = self.blocked(&Scope::New);
        for (id, book) in &mut state.notebooks {
            book.pending = self.blocked(&Scope::Input(id.clone()));
        }
    }
}
