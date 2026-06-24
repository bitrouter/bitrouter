//! PTY-backed terminal element on GPUI.
//!
//! Three layers, adapted (minimally) from Zed's terminal:
//! - [`terminal`]: the [`terminal::Terminal`] entity wrapping
//!   `alacritty_terminal::Term` behind a `FairMutex`, driven by alacritty's own
//!   PTY event loop.

pub mod terminal;
