//! PTY-backed terminal element on GPUI.
//!
//! Three layers, adapted (minimally) from Zed's terminal:
//! - [`entity`]: the [`entity::Terminal`] entity wrapping
//!   `alacritty_terminal::Term` behind a `FairMutex`, driven by alacritty's own
//!   PTY event loop.
//! - [`element`]: a gpui [`gpui::Element`] that paints a
//!   [`entity::TerminalSnapshot`] as a monospace grid.
//! - [`view`]: a [`gpui::Render`] view that forwards key events to the terminal
//!   and resizes it to fit the painted bounds.

pub mod element;
pub mod entity;
pub mod view;
