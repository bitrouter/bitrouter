//! PTY-backed terminal element on GPUI.
//!
//! Three layers, adapted (minimally) from Zed's terminal:
//! - [`terminal`]: the [`terminal::Terminal`] entity wrapping
//!   `alacritty_terminal::Term` behind a `FairMutex`, driven by alacritty's own
//!   PTY event loop.
//! - [`element`]: a gpui [`gpui::Element`] that paints a
//!   [`terminal::TerminalSnapshot`] as a monospace grid.
//! - [`view`]: a [`gpui::Render`] view that forwards key events to the terminal
//!   and resizes it to fit the painted bounds.

pub mod element;
pub mod terminal;
pub mod view;
