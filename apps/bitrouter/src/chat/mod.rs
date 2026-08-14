//! The app half of `bitrouter chat` — everything the renderer cannot own.
//!
//! `bitrouter-tui` draws; it does not read. Keys, raw mode, and the session's
//! lifetime belong here, because they are properties of *this* process rather
//! than of anything ACP carries.

pub mod input;
