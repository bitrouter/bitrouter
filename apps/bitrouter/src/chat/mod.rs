//! The app half of `bitrouter chat` — everything the renderer cannot own.
//!
//! `bitrouter-tui` draws; it does not read. Keys, raw mode, and the session's
//! lifetime belong here, because they are properties of *this* process rather
//! than of anything ACP carries.
//!
//! # The three exits
//!
//! The session holds raw mode from its first prompt (see [`input::Stdin`]), so
//! every way out has to give it back. There are three, and each reaches
//! [`crate::tui::lifecycle::restore`] by a different route:
//!
//! | exit | route |
//! |---|---|
//! | normal — Ctrl-C or Ctrl-D at an idle prompt, or stdin ending | `Stdin`'s `Drop`, which also covers every `?` out of `chat` |
//! | panic | `lifecycle::install_panic_restore`, chained in front of the existing hook |
//! | INT / TERM / HUP | `lifecycle::Shutdown` as a `select!` arm, in both the prompt and the turn loop |
//!
//! ## Checking it by hand
//!
//! The unit tests cover the arm and the hook, not the terminal: raw mode needs
//! a real tty, and a test that took one would take the *developer's*. So the
//! terminal itself is checked by hand, once per exit. In each case `stty -a`
//! afterwards must report `echo` and `icanon` (a restored terminal), not
//! `-echo` and `-icanon`, and the shell must still echo what you type:
//!
//! ```text
//! bitrouter chat <agent>   # then Ctrl-D at the prompt   → stty -a | grep -o '\-\?echo'
//! bitrouter chat <agent>   # then Ctrl-C at the prompt   → stty -a | grep -o '\-\?echo'
//! bitrouter chat <agent> & ; kill -TERM %1               → stty -a | grep -o '\-\?echo'
//! bitrouter chat <agent> & ; kill -HUP  %1               → stty -a | grep -o '\-\?echo'
//! ```
//!
//! The panic exit has no keystroke to trigger it; it is covered by
//! `lifecycle`'s `the_panic_hook_restores_and_still_reports`, which pins that
//! `restore` runs *and* that the panic is still reported afterwards.

pub mod cost;
pub mod input;
