//! Known control action identities and their protocol requirements.
//!
//! These describe the wire contract. The server independently decides which
//! actions are configured and which grants the authenticated caller holds.

use super::ControlScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Models,
    Route,
    Requests,
    Providers,
    Observe,
    PolicyStatus,
    PolicyShow,
    Agents,
    Reload,
}

impl Action {
    /// The key used in `CapabilitiesReport::action_descriptors`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Models => "list_models",
            Self::Route => "route",
            Self::Requests => "requests",
            Self::Providers => "providers_list",
            Self::Observe => "observe_status",
            Self::PolicyStatus => "policy_status",
            Self::PolicyShow => "policy_show",
            Self::Agents => "agents_list",
            Self::Reload => "reload",
        }
    }

    /// Resolve a descriptor key without accepting unknown future operations.
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "status" => Self::Status,
            "list_models" => Self::Models,
            "route" => Self::Route,
            "requests" => Self::Requests,
            "providers_list" => Self::Providers,
            "observe_status" => Self::Observe,
            "policy_status" => Self::PolicyStatus,
            "policy_show" => Self::PolicyShow,
            "agents_list" => Self::Agents,
            "reload" => Self::Reload,
            _ => return None,
        })
    }

    /// Name in the original capability list, before versioned descriptors.
    pub const fn legacy_name(self) -> &'static str {
        match self {
            Self::Models => "models",
            Self::Route => "route_preview",
            _ => self.id(),
        }
    }

    /// Descriptor version understood for this action.
    pub const fn version(self) -> u32 {
        1
    }

    pub const fn required_scope(self) -> ControlScope {
        match self {
            Self::Reload => ControlScope::Reload,
            _ => ControlScope::Read,
        }
    }

    /// Only these original reads can be discovered without scoped descriptors.
    pub const fn supports_legacy_discovery(self) -> bool {
        matches!(
            self,
            Self::Status | Self::Models | Self::Route | Self::Requests
        )
    }
}
