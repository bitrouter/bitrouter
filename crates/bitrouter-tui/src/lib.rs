//! Terminal renderer for one ACP agent session.
//!
//! # The boundary
//!
//! **This crate must never depend on `bitrouter`, the application.** That
//! absence is the point, and it is enforced by the build rather than by
//! prose: everything rendered here arrives over the ACP wire, and anything
//! that cannot be learned that way is passed in by the caller.
//!
//! The previous terminal UI died of accretion — it lived inside the
//! application, so it could reach any function in it, and it grew verbs with
//! no command-line equivalent until nobody could say what it was. A module
//! boundary is a promise; a crate boundary is a compiler error. This is the
//! same rule, made checkable.
//!
//! Two consequences follow, and both are deliberate:
//!
//! - **Session-scoped only.** There is no metering store here, no daemon
//!   control socket, no request history — nothing daemon-wide is reachable,
//!   so nothing daemon-wide can be drawn. What one session did is the whole
//!   subject.
//! - **Generality is a consequence, not a goal.** Conforming to ACP is what
//!   makes this renderer work against agents other than BitRouter; it is not
//!   a feature paid for separately. Anything the protocol does not carry is
//!   negotiated, not assumed — see [`Capabilities`].
//!
//! # Honesty
//!
//! A control that does not do what it appears to do is worse than an absent
//! one. A figure whose scope is unknown is worse than a blank. Both rules are
//! why [`Capabilities`] exists: the renderer asks what this session can
//! actually honor, and draws only that.

pub mod cost;
pub mod journal;
pub mod log_tail;
pub mod permission;
pub mod picker;
pub mod transcript;
pub mod viewport;
pub mod wrap;

use agent_client_protocol_schema::v1::AgentCapabilities;

/// The optional surfaces this session's agent advertised.
///
/// ACP is a protocol many agents implement partially. BitRouter serves the
/// router-specific ones — a routing catalog, measured cost — but a generic
/// agent serves none of them, and a renderer that draws a provider picker
/// against an agent with no `providers/*` would be offering a control that
/// does nothing.
///
/// So each optional pane is gated on a fact, not a guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// The agent answers `providers/list` and `providers/set`, so a
    /// provider picker has something to talk to.
    pub providers: bool,
    /// The agent can load a previously saved session.
    ///
    /// Read straight from the standard `loadSession` capability rather than
    /// inferred, so an agent that gains it later needs no change here.
    pub load_session: bool,
}

impl Capabilities {
    /// Read what the agent advertised at `initialize`.
    ///
    /// `providers` is not a field of [`AgentCapabilities`] in ACP v1 — the
    /// methods live behind the schema crate's `unstable_llm_providers`
    /// feature, and the runtime crate does not forward it — so it is reported
    /// separately by the caller, which knows whether the probe succeeded.
    pub fn from_agent(caps: &AgentCapabilities, providers: bool) -> Self {
        Self {
            providers,
            load_session: caps.load_session,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unadvertised_surface_is_never_assumed() {
        // The default must be "draws nothing optional". A renderer that
        // defaulted to on would show dead controls against every agent that
        // has not implemented them, which is most of them.
        let caps = Capabilities::default();
        assert!(!caps.providers);
        assert!(!caps.load_session);
    }

    #[test]
    fn capabilities_follow_what_the_agent_actually_said() {
        let mut advertised = AgentCapabilities::new();
        advertised.load_session = true;

        let with_providers = Capabilities::from_agent(&advertised, true);
        assert!(with_providers.load_session);
        assert!(
            with_providers.providers,
            "a probed `providers/*` surface is reported by the caller"
        );

        // The same agent, with no routing surface behind it: the picker must
        // stay off even though everything else is identical.
        let without = Capabilities::from_agent(&advertised, false);
        assert!(without.load_session);
        assert!(!without.providers);
    }
}
