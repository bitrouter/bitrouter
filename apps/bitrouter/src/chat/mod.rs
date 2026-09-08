//! App-level support for the canonical Code conversation and piped `chat`.
//!
//! The internal `code` module owns the only full-screen interaction loop, including terminal
//! restoration, prompt lifetime, and permission handling. [`session`] keeps
//! the plain compatibility renderer and session-log helpers for redirected
//! legacy invocations. [`effects`] remains the shared ACP wire interpreter for
//! headless and plain execution.

pub(crate) mod code;
pub(crate) mod code_controls;
pub(crate) mod code_wire;
pub(crate) mod editor;
pub mod effects;
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
            ("code.rs", include_str!("code.rs")),
            ("code_wire.rs", include_str!("code_wire.rs")),
            ("code_controls.rs", include_str!("code_controls.rs")),
            ("editor.rs", include_str!("editor.rs")),
            ("effects.rs", include_str!("effects.rs")),
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
            // The driver renders reports it may not name. A type it cannot
            // name it cannot store, poll, or put in the footer — which is how
            // "rendered once, on request, never retained" is enforced rather
            // than merely intended. It deals in `Box<dyn CliReport>`.
            "StatusReport",
            "ModelsReport",
            "RouteReport",
            "SkillsReport",
            "CommandsReport",
            // Action rendering is palette-free. The only renderer the driver
            // may call is `render_to_vec`, which hard-codes `Theme::none()`;
            // a themed render would write raw ANSI into the differential
            // writer's screen.
            "for_stdout",
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
