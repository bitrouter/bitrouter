//! Typed BitRouter actions shared by the CLI, Code session, and HTTP control
//! surfaces.
//!
//! Each action owns one report type and, where dependency injection is useful,
//! one port trait. Implementations live beside those contracts in this module.
//! MCP origin metadata does not belong here: the OSS origin server was removed,
//! while the independent upstream MCP gateway remains in `bitrouter-sdk`.

pub mod administration;
pub mod checks;
pub(crate) mod code;
pub mod commands;
pub mod models;
pub mod panel;
pub mod requests;
pub mod route;
pub mod session;
pub mod skills;
pub mod status;

/// A typed action failed.
///
/// The action boundary deliberately carries only a presentation-neutral
/// message. CLI, TUI, and HTTP adapters decide how to envelope and render it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    /// Build an action error from anything string-like.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Whether an action observes or changes state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Safe to run at any time, including twice.
    Read,
    /// Changes state. `inverse` names the action that undoes it.
    Write { inverse: &'static str },
}

/// What a live session must hold before offering an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requires {
    /// Offered in every session.
    Nothing,
    /// Needs the controller's route-control binding.
    Binding,
}

/// How far an answer travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Answerable through the CLI and scoped HTTP control surface.
    Portable,
    /// Resolves against this machine's config, socket, or filesystem.
    HostBound,
    /// Its subject is one live session.
    SessionBound,
}

/// One action and the non-MCP surfaces that answer it.
pub struct ActionSpec {
    /// Stable action id.
    pub id: &'static str,
    /// Canonical CLI leaf, when a separate process can answer the question.
    pub cli_leaf: Option<&'static str>,
    /// Code-session command without the leading slash.
    pub tui_command: Option<&'static str>,
    /// Whether the action reads or writes.
    pub effect: Effect,
    /// Session capability needed before it can be offered.
    pub requires: Requires,
    /// The action's ownership boundary.
    pub reach: Reach,
}

/// Actions shared by more than one retained OSS surface.
pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        id: "status",
        cli_leaf: Some("status"),
        tui_command: Some("status"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::Portable,
    },
    ActionSpec {
        id: "list_models",
        cli_leaf: Some("models"),
        tui_command: Some("models"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::Portable,
    },
    ActionSpec {
        id: "route",
        cli_leaf: Some("route"),
        tui_command: Some("preview"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::HostBound,
    },
    ActionSpec {
        id: "commands",
        cli_leaf: Some("acp commands"),
        tui_command: Some("commands"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::SessionBound,
    },
    ActionSpec {
        id: "route_set",
        cli_leaf: None,
        tui_command: Some("route"),
        effect: Effect::Write {
            inverse: "route_reset",
        },
        requires: Requires::Binding,
        reach: Reach::SessionBound,
    },
    ActionSpec {
        id: "route_reset",
        cli_leaf: None,
        tui_command: Some("route reset"),
        effect: Effect::Write {
            inverse: "route_set",
        },
        requires: Requires::Binding,
        reach: Reach::SessionBound,
    },
];

#[cfg(test)]
mod tests {
    use super::{ACTIONS, Effect, Reach};

    #[test]
    fn every_session_action_is_fully_inventoried() {
        for action in ACTIONS {
            let Some(command) = action.tui_command else {
                continue;
            };
            assert!(
                action.cli_leaf.is_some() || action.reach == Reach::SessionBound,
                "`{}` is offered as `/{command}` without a CLI leaf, but is not session-bound",
                action.id,
            );
        }
    }

    #[test]
    fn every_write_names_a_reciprocal_inverse() {
        for action in ACTIONS {
            let Effect::Write { inverse } = action.effect else {
                continue;
            };
            let row = ACTIONS.iter().find(|candidate| candidate.id == inverse);
            assert!(
                row.is_some(),
                "`{}` names missing inverse `{inverse}`",
                action.id
            );
            let inverse_details = row.and_then(|row| match row.effect {
                Effect::Write { inverse } => Some((inverse, row.tui_command)),
                Effect::Read => None,
            });
            assert!(
                inverse_details.is_some(),
                "`{inverse}` is not a write action"
            );
            let Some((back, inverse_command)) = inverse_details else {
                continue;
            };
            assert_eq!(back, action.id);
            assert!(
                action.tui_command.is_none() || inverse_command.is_some(),
                "a session-visible write must have a session-visible inverse"
            );
        }
    }
}
