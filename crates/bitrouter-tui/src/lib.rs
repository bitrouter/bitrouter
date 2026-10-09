//! Terminal renderers for BitRouter's coding conversation and operations surfaces.
//!
//! # The boundary, and what it is now for
//!
//! **This crate must never depend on `bitrouter`, the application.** Cargo
//! enforces it — the app depends on this crate by path, so the reverse edge is
//! a cyclic-package error rather than a matter of review.
//!
//! The previous terminal UI died of accretion. It lived inside the
//! application, so it could reach any function in it, and it grew verbs with
//! no command-line equivalent until nobody could say what it was. That is
//! still the failure this boundary prevents: what it stops is *reachability*,
//! not vocabulary.
//!
//! # What this crate stopped claiming
//!
//! It was once ACP-generic: no BitRouter name appeared here, so it would
//! render any conforming agent. That charter is **retired**. This is
//! BitRouter's TUI, and it may name BitRouter's concepts — the
//! [`cost::COST_PROVENANCE_META_KEY`] wire spelling is the first, and it
//! lives here because splitting one key across a crate boundary bought
//! nothing but two places to look.
//!
//! What is *not* retired is the reason genericity was chosen in the first
//! place. ACP sessions retain their protocol projection; native BRO sessions
//! receive plain presentation data from the application. The shared writer,
//! composer and menu do not own transport or execution.
//!
//! # Honesty
//!
//! A control that does not do what it appears to do is worse than an absent
//! one. A figure whose scope is unknown is worse than a blank. These are the
//! rules that survived the charter, and they are pinned by tests rather than
//! by an absent dependency:
//!
//! - A cost nobody can vouch for is never drawn as ours: the harness's own
//!   figure is labelled the agent's, an unknown marker is not drawn, and no
//!   figure at all renders as *unreported*, never as `$0.00`
//!   ([`cost::from_usage`] is the only way in).
//! - A controller that advertises no route control gets no picker at all,
//!   not a dead one ([`picker::Picker::open`] returns `None`).
//! - Cancelling a permission prompt can never resolve to consent.
//!
//! Where honesty depends on something the protocol does not carry, that
//! something is still a **parameter** and not an inference: there is no
//! constructor that skips the question. [`cost::Cost::new`] takes the
//! provenance; [`picker::Picker::open`] takes whether the surface exists.
//!
//! # What is still out of scope
//!
//! There is no metering store here, no daemon control socket, and no HTTP
//! client. Session updates and operation reports are plain input supplied by
//! the application, so drawing can never become a second implementation of an
//! action.

pub mod agents;
pub mod agents_menu;
pub mod code;
pub mod composer;
pub mod cost;
pub mod editor;
pub mod journal;
pub mod lifecycle;
pub mod log_tail;
pub mod machine;
pub mod native_agent;
pub mod permission;
pub mod picker;
pub mod plain;
pub mod render;
pub mod select;
pub mod view;
pub mod wrap;
pub mod writer;
