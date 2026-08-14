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
//!   the caller's to supply, not this crate's to assume.
//!
//! # Honesty
//!
//! A control that does not do what it appears to do is worse than an absent
//! one. A figure whose scope is unknown is worse than a blank.
//!
//! The rule is kept by where things live rather than by a capability struct.
//! This crate draws what a session update actually carried; anything gated on
//! what an agent can honour — a provider picker, a route line — is composed by
//! the caller into the footer, because the caller is what knows whether the
//! control would do anything.

pub mod journal;
pub mod log_tail;
pub mod permission;
pub mod picker;
pub mod render;
pub mod wrap;
pub mod writer;
