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
//! Where a control's honesty depends on something the protocol does not carry,
//! that something is a **parameter**, not an inference — and there is no
//! constructor that skips it. [`picker::Picker::open`] takes whether the agent
//! serves `providers/*` at all; [`cost::Cost::new`] takes whose spend the
//! figure is. The caller answers, because the caller is what knows; this crate
//! makes not answering impossible rather than merely discouraged.
//!
//! # The line the boundary is drawn on
//!
//! *Knowledge*, not medium. Rendering something is never by itself a reason to
//! move it out; naming something that is not in the protocol always is. So
//! `providers/list` renders here — [`ProviderInfo`] is an ACP schema type — but
//! the `_meta` key BitRouter invents to carry cost scope stays in the app, and
//! `grep -rn "bitrouter/" crates/bitrouter-tui/src` returning nothing is how
//! that is checked.
//!
//! [`ProviderInfo`]: agent_client_protocol_schema::v1::ProviderInfo

pub mod cost;
pub mod editor;
pub mod journal;
pub mod lifecycle;
pub mod log_tail;
pub mod permission;
pub mod picker;
pub mod plain;
pub mod render;
pub mod view;
pub mod wrap;
pub mod writer;
