//! The app half of `bro chat` — everything the renderer cannot own.
//!
//! `bitrouter-tui` draws; it does not read. Keys, raw mode, and the session's
//! lifetime belong here, because they are properties of *this* process rather
//! than of anything ACP carries.
//!
//! # The three exits
//!
//! The session holds raw mode from its first prompt (see [`input::Stdin`]), so
//! every way out has to give it back. There are three, and each reaches
//! [`bitrouter_tui::lifecycle::restore`] by a different route:
//!
//! | exit | route |
//! |---|---|
//! | normal — Ctrl-C or Ctrl-D at an idle prompt, or stdin ending | `session::run` drops `Stdin` after teardown; `Stdin`'s `Drop` also covers every `?` out of `chat` |
//! | panic | `bitrouter_tui::lifecycle::install_panic_restore`, chained in front of the existing hook |
//! | INT / TERM / HUP | [`signals::Shutdown`] as the one `select!` arm in `session::run`'s single loop |
//!
//! Giving the terminal back is only half of an exit. The other half is the
//! harness child and this session's route leases, and those are
//! `ControlledSession::shutdown`'s — which is why the loop lives in a function
//! whose errors `session::run` *carries* rather than returns. A `?` that
//! escaped would restore the terminal (`Drop` sees to that) and leave the child
//! unreaped.
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
//! bro chat <agent>   # then Ctrl-D at the prompt   → stty -a | grep -o '\-\?echo'
//! bro chat <agent>   # then Ctrl-C at the prompt   → stty -a | grep -o '\-\?echo'
//! bro chat <agent> & ; kill -TERM %1               → stty -a | grep -o '\-\?echo'
//! bro chat <agent> & ; kill -HUP  %1               → stty -a | grep -o '\-\?echo'
//! ```
//!
//! The panic exit has no keystroke to trigger it; it is covered by
//! `lifecycle`'s `the_panic_hook_restores_and_still_reports`, which pins that
//! `restore` runs *and* that the panic is still reported afterwards.

pub mod effects;
pub mod input;
pub mod session;
pub mod signals;

#[cfg(test)]
mod tests {
    /// The guard §8 asks for, because what stays here is rendering-adjacent
    /// code sitting where it *could* reach anything.
    ///
    /// `picker.rs` and the rendering half of `cost.rs` went back to
    /// `bitrouter-tui`, where the compiler keeps them honest. What is left
    /// here is the part that genuinely is not ACP — this process's stdin,
    /// its signals, its session log — and it gets this instead: the chat
    /// module may read the ACP wire and the terminal, and nothing else. No
    /// `Config`, no metering store, no control socket, and none of the
    /// daemon bridges the launch half builds — the route surface it drives
    /// is `_bitrouter/route/*` on the shared client, which *is* the wire.
    /// The one handle it holds on the launch half is the session's own
    /// teardown, because the session's lifetime is this module's charter.
    ///
    /// Checked against the sources themselves rather than by review, so it
    /// fails the build that breaks it instead of the review that misses it.
    #[test]
    fn the_chat_module_reaches_nothing_daemon_wide() {
        let sources = [
            ("effects.rs", include_str!("effects.rs")),
            ("input.rs", include_str!("input.rs")),
            ("session.rs", include_str!("session.rs")),
        ];
        // Spelled as paths and type names, so a mention in prose — "the
        // daemon's total" — is not a false positive. `session.rs` states in
        // prose what it does not reach, for exactly that reason.
        let forbidden = [
            "crate::daemon",
            "crate::metering",
            "crate::policy",
            "MeteringStore",
            "bitrouter_sdk::config",
            "control_socket",
            "DaemonRouteControl",
            "DaemonSessionCost",
            "LocalControllerBinding",
        ];
        for (name, source) in sources {
            for reach in forbidden {
                assert!(
                    !source.contains(reach),
                    "{name} reaches `{reach}`; the chat module draws what ACP \
                     carries and what the terminal is, and nothing else"
                );
            }
        }
    }
}
