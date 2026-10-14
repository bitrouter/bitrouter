# Initial task model route selection

Task resource selection can now carry an optional BRO `model_route`. The
planning inventory contains routes that resolve against current host
configuration and configured adapters expected to expose the existing BRO
session route controller. Startup still checks the actual negotiated extension;
configuration or an adapter package name is not acknowledgment.

The host installs the selected session route after opening the ACP session and
before its first work prompt. A missing capability, failed request, cancellation,
or mismatched response stops startup and reaps the owned adapter. The supervisor
retains its acknowledged route. The Goal coordinator copies that fact into the
matching execution through a durable transition checking execution ID, actual
run mapping, dispatched state, source requirements and the frozen request.
Read-only branch details distinguish requested and acknowledged routes.

Route selectors do not identify the actual upstream provider model. The
optional route controller is currently supplied by existing configured adapter
wiring; generic ACP workflows remain available without that optional control.
Private harness model settings, adaptive quality/cost judgment, live model
replacement and real-provider evidence remain separate work. The fixture uses
the maintained adapter invocation marker to exercise controller wiring, but
the executable is a Cargo-built test peer, not the real Codex adapter.

The first `cargo check` passed, but preceded the completed tests. The initial
v2 nextest build was deliberately interrupted (exit 130) while tests were being
finished; it executed no tests and is not a result. v3 uses a frozen 102-file
source manifest and the actual coordinator, database, control socket and Rust
ACP peer. Its run result is recorded separately; no fixed fixture judgment
establishes model selection quality.

The repository skill, CLI reference and all three plugin manifests were
updated with this harness wiring change. Permission and budget admission
remain unchanged; resource selection does not authorize additional actions.

v3 run `de5b0e6d-0be4-4b53-9ceb-f47ffebb4071` passed all 74 selected cases,
exit 0, 137.200 seconds, two slow cases and no LEAK/timeout. The positive
coordinator case used actual provider endpoint setup, session route installation
through the daemon control socket, parallel ACP work and verified delivery.
The negative case confirmed that absent route control sent no prompt and reaped
the actual peer. Database coverage rejected missing/wrong run mappings and
wrong routes, then preserved the acknowledged route through reload. All
original Goal database and six coordinator scenarios in this filter passed.
2,201 unrelated cases were filtered; this is not a full workspace pass.

v4 adds an explicit Unix condition to the process fixture test (its helper and
coordinator fixture are Unix-only), and makes route evidence labels readable
without Rust Option debug syntax. The read-only summary distinguishes BRO route
selection from unknown upstream identity. Final workspace/static/doc outcomes
are recorded against the separate v4 source manifest.

v4 full workspace run `cb0a8f9e-d150-4f74-821e-c09379509ca2` passed all 4,564
executed tests across 70 binaries, exit 0, 636.435 seconds, nine slow cases,
22 existing opt-in tests skipped and no LEAK or timeout. Original native,
protocol, permission, recovery, TUI and capacity cases were retained. This
fixture/full-workspace result does not satisfy the real harness/provider/typed
backend or input-quality gates.

Read-only inspection of the existing local configuration found configured
Claude/Codex ACP entries and no `decision_model`. No credential values were
printed, persisted in this evidence or requested through chat. An asynchronous
question requests an existing typed-backend configuration path or endpoint,
model ID and credential environment-variable name; independent work continues.

Final combined session 72410 exited 0. Workspace/all-target/all-feature Clippy
with warnings denied passed in 1 minute 12 seconds. Separate doctests passed six
cases across seven suites, with one existing ignored example. Formatting and
diff checks passed, and all 102 v4 source hashes matched. The final process
inventory found no owned Cargo/rustc or ACP/PTY fixture processes remaining.
