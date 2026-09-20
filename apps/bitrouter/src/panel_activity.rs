//! Owner-scoped lifecycle state for the local menu-bar companion.
//!
//! Routed traffic can prove recent request activity, but it cannot prove that
//! an agent is waiting for approval or that a turn completed. Managed ACP
//! clients publish those stronger facts over the existing owner-only control
//! socket. The daemon retains a bounded snapshot and transition log so the
//! companion can observe short-lived states even while its menu is closed.

use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const DEFAULT_STATE_TTL: Duration = Duration::from_secs(35);
const DEFAULT_EVENT_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_EVENTS: usize = 256;
const MAX_FIELD_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleState {
    Connecting,
    Idle,
    Working,
    NeedsApproval,
    Completed,
    Failed,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentActivityUpdate {
    pub instance_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub state: AgentLifecycleState,
    pub activity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentActivitySnapshot {
    pub instance_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub state: AgentLifecycleState,
    pub activity: String,
    /// When the visible lifecycle state or activity last changed.
    pub updated_at: DateTime<Utc>,
    /// Latest heartbeat for expiry only. Kept out of the companion payload so
    /// a repeated heartbeat cannot make an old transition look new.
    pub(crate) last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentActivityEvent {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub state: AgentLifecycleState,
    pub activity: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Default)]
struct ActivityState {
    current: HashMap<String, AgentActivitySnapshot>,
    events: VecDeque<AgentActivityEvent>,
}

pub struct AgentActivityRegistry {
    state: RwLock<ActivityState>,
    state_ttl: Duration,
    event_ttl: Duration,
}

impl Default for AgentActivityRegistry {
    fn default() -> Self {
        Self {
            state: RwLock::new(ActivityState::default()),
            state_ttl: DEFAULT_STATE_TTL,
            event_ttl: DEFAULT_EVENT_TTL,
        }
    }
}

impl AgentActivityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            state: RwLock::new(ActivityState::default()),
            state_ttl: ttl,
            event_ttl: ttl,
        }
    }

    pub fn publish(&self, update: AgentActivityUpdate) -> Result<(), String> {
        self.publish_at(update, Utc::now())
    }

    fn publish_at(&self, update: AgentActivityUpdate, now: DateTime<Utc>) -> Result<(), String> {
        validate_field("instance id", &update.instance_id)?;
        validate_field("agent id", &update.agent_id)?;
        validate_field("session id", &update.session_id)?;
        validate_field("activity", &update.activity)?;

        let mut state = self.write_state();
        cleanup(&mut state, now, self.state_ttl, self.event_ttl);
        let changed = state
            .current
            .get(&update.instance_id)
            .is_none_or(|previous| {
                previous.agent_id != update.agent_id
                    || previous.session_id != update.session_id
                    || previous.state != update.state
                    || previous.activity != update.activity
            });
        if changed {
            let snapshot = AgentActivitySnapshot {
                instance_id: update.instance_id.clone(),
                agent_id: update.agent_id.clone(),
                session_id: update.session_id.clone(),
                state: update.state,
                activity: update.activity.clone(),
                updated_at: now,
                last_seen_at: now,
            };
            state.current.insert(update.instance_id.clone(), snapshot);
        } else if let Some(snapshot) = state.current.get_mut(&update.instance_id) {
            snapshot.last_seen_at = now;
        }
        if changed && records_notification_event(update.state) {
            state.events.push_back(AgentActivityEvent {
                id: format!("braevt_{}", uuid::Uuid::new_v4().simple()),
                agent_id: update.agent_id,
                session_id: update.session_id,
                state: update.state,
                activity: update.activity,
                occurred_at: now,
            });
            while state.events.len() > MAX_EVENTS {
                state.events.pop_front();
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> (Vec<AgentActivitySnapshot>, Vec<AgentActivityEvent>) {
        self.snapshot_at(Utc::now())
    }

    fn snapshot_at(
        &self,
        now: DateTime<Utc>,
    ) -> (Vec<AgentActivitySnapshot>, Vec<AgentActivityEvent>) {
        let mut state = self.write_state();
        cleanup(&mut state, now, self.state_ttl, self.event_ttl);
        let mut current = state.current.values().cloned().collect::<Vec<_>>();
        current.sort_by_key(|activity| std::cmp::Reverse(activity.updated_at));
        (current, state.events.iter().cloned().collect())
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, ActivityState> {
        match self.state.write() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn validate_field(label: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("agent activity {label} must not be empty"));
    }
    if value.len() > MAX_FIELD_BYTES {
        return Err(format!("agent activity {label} is too long"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "agent activity {label} contains control characters"
        ));
    }
    Ok(())
}

fn records_notification_event(state: AgentLifecycleState) -> bool {
    matches!(
        state,
        AgentLifecycleState::NeedsApproval
            | AgentLifecycleState::Completed
            | AgentLifecycleState::Failed
            | AgentLifecycleState::Disconnected
    )
}

fn cleanup(
    state: &mut ActivityState,
    now: DateTime<Utc>,
    state_ttl: Duration,
    event_ttl: Duration,
) {
    let (Ok(state_ttl), Ok(event_ttl)) = (
        chrono::Duration::from_std(state_ttl),
        chrono::Duration::from_std(event_ttl),
    ) else {
        state.current.clear();
        state.events.clear();
        return;
    };
    let state_cutoff = now - state_ttl;
    let event_cutoff = now - event_ttl;
    state
        .current
        .retain(|_, activity| activity.last_seen_at > state_cutoff);
    state
        .events
        .retain(|event| event.occurred_at > event_cutoff);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(state: AgentLifecycleState, activity: &str) -> AgentActivityUpdate {
        AgentActivityUpdate {
            instance_id: "code-process".into(),
            agent_id: "codex-acp".into(),
            session_id: "native-session".into(),
            state,
            activity: activity.into(),
        }
    }

    #[test]
    fn heartbeats_refresh_snapshot_without_duplicating_events() -> Result<(), String> {
        let registry = AgentActivityRegistry::with_ttl(Duration::from_secs(10));
        let started = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(100);
        registry.publish_at(
            update(AgentLifecycleState::Working, "Running tests"),
            started,
        )?;
        registry.publish_at(
            update(AgentLifecycleState::Working, "Running tests"),
            started + chrono::Duration::seconds(8),
        )?;
        let (current, events) = registry.snapshot_at(started + chrono::Duration::seconds(8));
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].updated_at, started);
        assert_eq!(
            current[0].last_seen_at,
            started + chrono::Duration::seconds(8)
        );
        assert!(events.is_empty());

        registry.publish_at(
            update(AgentLifecycleState::NeedsApproval, "Approval needed"),
            started + chrono::Duration::seconds(9),
        )?;
        let (_, events) = registry.snapshot_at(started + chrono::Duration::seconds(9));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, AgentLifecycleState::NeedsApproval);
        Ok(())
    }

    #[test]
    fn heartbeat_liveness_expires_independently_of_transition_time() -> Result<(), String> {
        let ttl = Duration::from_secs(10);
        let registry = AgentActivityRegistry::with_ttl(ttl);
        let started = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(100);
        registry.publish_at(update(AgentLifecycleState::Completed, "Done"), started)?;
        registry.publish_at(
            update(AgentLifecycleState::Completed, "Done"),
            started + chrono::Duration::seconds(8),
        )?;

        let mut state = registry.write_state();
        cleanup(
            &mut state,
            started + chrono::Duration::seconds(17),
            ttl,
            ttl,
        );
        assert_eq!(state.current.len(), 1);
        cleanup(
            &mut state,
            started + chrono::Duration::seconds(19),
            ttl,
            ttl,
        );
        assert!(state.current.is_empty());
        Ok(())
    }

    #[test]
    fn invalid_untrusted_labels_are_rejected() {
        let registry = AgentActivityRegistry::new();
        let mut invalid = update(AgentLifecycleState::Working, "working");
        invalid.agent_id = "codex\nspoofed".into();
        assert!(registry.publish(invalid).is_err());
    }
}
