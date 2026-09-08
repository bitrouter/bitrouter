# Remote administration implementation ledger

Objective: implement the whole [remote administration spec](REMOTE_ADMINISTRATION_SPEC.md)
and verify its acceptance criteria. Authorized 2026-09-08. Baseline `b657cb62`.

Requested workers: `gpt-5.3-codex-spark`, `xhigh`; launch requests were accepted,
but all three workers terminated at the Spark usage limit before making changes.
The maintainer selected `gpt-5.6-terra` at `max`; replacement workers completed the implementation.
Status: **implementation and prior spec acceptance are complete, but the
expanded live external-host MCP acceptance is currently blocked on 2026-09-08.**
The default Docker command reaches the real TLS sidecar and receives HTTP 403
at MCP `initialize` because rmcp rejects `Host: server:8443`; `tools/list` and
the advertised MCP tool calls therefore cannot execute. The historical notes
below remain evidence for the prior specification acceptance. A separately
labeled selected run passes the independent CLI, HTTP, reload, and dashboard
journeys with MCP intentionally skipped; it is not complete remote acceptance.

| Requirement | State | Evidence |
| --- | --- | --- |
| A: canonical remote inventory and schema/profile guards | Complete | Canonical action/resource rows and schema identity tests pass; existing MCP profile guards remain intact. |
| A: shared CLI/dashboard target ports, no local fallback | Complete | InspectionTarget is shared; real subprocess traps prove denied/failed remote actions do not access local config, metering or sockets. |
| A: additive discovery and old/new compatibility | Complete | Legacy names, old-server read compatibility, scope discovery and no-redirect HTTP tests pass. |
| A: REST/MCP caller validation and shared disclosure | Partial | Loopback HTTP MCP validation uses the same redacted read ports. Expanded Docker TLS `initialize` is rejected before runtime inventory or tool calls by rmcp's Host allowlist. |
| B: providers, telemetry, passive agent reads | Complete | Live typed ports and redaction tests pass; all advertised reads exercised over Docker HTTP and CLI. |
| B: active/disk policy status/detail | Complete | Snapshot mode, bindings and digest tests pass; Docker observes disk/live divergence and convergence after reload. |
| B: bounded request filters, summaries, availability | Complete | Storage and report tests plus Docker CLI/HTTP filters, truncation, validation and redaction pass. |
| B: CLI and dashboard all read actions | Complete | Selected Docker run exercises all nine CLI leaves and semantic dashboard Home, Models, Requests, Route, Providers, Telemetry, Agents, active/disk policy overview/detail, and excluded remote ACP views. |
| C: unified reload ownership and local env serialization | Complete | HTTP/local IPC/SIGHUP admission race and reservation tests pass. |
| C: prepared inputs and restart-required classification | Complete | All five participants prepare before mutation; immutable startup baseline, nested unknown fields and 60-second preparation deadline are tested. |
| C: per-participant failure and mixed runtime state | Complete | Fault matrix, immutable prepared inputs, active digest, mixed-history and interruption-truth tests pass. |
| D: explicit scoped credentials, legacy read token | Complete | Named read/reload credentials, owner isolation and legacy read-only fallback pass unit/HTTP/Docker tests. |
| D: guarded reload admission and retained operations | Complete | Generation/boot guards, deduplication, ownership, 1024-entry capacity and 24-hour retention tests pass. |
| D: CLI/dashboard reload and operation recovery | Complete | Selected Docker run verifies a successful production CLI reload plus `operations show` recovery, receipt-body loss recovery, boot mismatch, and successful/partial PTY outcomes. |
| D: limits, audit events, HTTP errors | Complete | 16 KiB input, 8 MiB report bound, advertised limits, structured audit and safe error tests pass. |
| E: inventory/HTTP/auth/version/redaction tests | Complete | Final all-feature suite includes HTTP scope/shape/owner, ambiguity, redirect, legacy and inventory-route guards. |
| E: target isolation and read divergence tests | Complete | Linux inotify, synthetic provider/config/metering inputs and two Unix-socket traps pass wrong-token and unreachable-context checks. |
| E: reload faults/concurrency/disconnect/boot tests | Complete | Core fault/admission tests plus actual Docker lost receipt-body, same-ID lookup, deduplication and restart acceptance pass. |
| E: dashboard and real terminal verification | Complete | Inventory-to-page guard and stale/scope tests pass; selected Docker PTY covers semantic read panels, route Backspace/Ctrl-U, policy Up/Down, typed detail, PageDown/PageUp and active/disk before reload; both outcomes restore terminal modes and subsequent shell output. |
| E: isolated Docker client/server TLS acceptance | Partial | Linux/ARM64 selected CLI/dashboard run passes and cleans up. Default full harness fails at external-host MCP `initialize` with HTTP 403; this is container, not physical-host, evidence. |
| E: CLI, shipped skill, config schemas and distribution | Complete | CLI and skill references updated; schema/dist, relative links and all eight extracted package builds pass. |
| E: all-feature tests, doc tests, clippy, fmt | Complete | Final suite: 3169 passed, 12 skipped; one existing ACP test had a non-failing leak annotation and passed cleanly on focused rerun. Doc tests, strict rustdoc, clippy and fmt pass. |

## Progress

- 2026-09-08: reread current worktree/spec/AGENTS.md; source tree initially
  unchanged except the authored design and docs index. Split bounded request,
  credential, and reload-core work among the requested model/effort workers.
- No requirement is marked complete without integration and verification.
- Spark workers all returned usage-limit errors before edits. Parent added the
  initial administration DTOs and policy-runtime inspection snapshot; compilation
  and transport integration are still in progress.
- Maintainer selected `gpt-5.6-terra` at `max`; three workers now own requests,
  reload coordination, and CLI/dashboard reads. Read-side all-target compilation
  passed before the next integration edits. Focused real HTTP MCP test passed;
  the complete focused suite is still being integrated.
- Maintainer selected Docker for final network-boundary acceptance. Docker
  Desktop 4.81.0 is running with a Linux arm64 engine. No physical-host deployment claim will be made from container evidence.
- Focused nextest run `e81c6cbc-3f37-41d8-9b2c-eff585c6ff5a`: 32 tests passed across administration reads, request filters, remote auth/MCP/schema/legacy compatibility, operation deduplication/retention/ownership, and reload core. This predates the final HTTP mutation and dashboard integration edits.
- Expanded focused suite: 35 passed, including HTTP reload body/scope guards, operation ownership and boot mismatch responses, and local target rejection.
- Shared live consistency headers now identify reads that may span reload generations, without claiming a single atomic runtime configuration.
- First workspace-wide `cargo nextest run --all-features --no-fail-fast` passed: 3,149 tests, 11 skipped, test execution 19.788 seconds. This successful build precedes the final asynchronous dashboard reload and Docker fault-fixture edits, which still require final verification.
- Reload preparation now has a 60-second deadline; commit has no cancellation deadline. New tests cover all five participant failure boundaries, immutable prepared inputs, mixed-history preservation, nested unknown configuration, and local IPC/SIGHUP entry methods racing a held remote reservation.
- Docker harness uses separate TLS client/server containers and a separate lib-test HTTP partial-failure fixture. The shipped server has no fault flags. A standard-library PTY harness checks explicit reload, visible partial state, quit, terminal modes, and subsequent shell output. Execution is pending the final integrated build; these assets are not yet acceptance evidence.
- Final integrated workspace run after reload fixes: `cargo nextest run
  --all-features --no-fail-fast` passed 3,168 tests, with 12 skipped, in
  30.156 seconds of test execution. This includes all five participant fault
  boundaries, local/remote/SIGHUP admission races, immutable startup-setting
  classification, prepared-input stability, and interrupted-outcome truth.
- `cargo test --doc --all-features` passed five documentation tests (one ignored);
  `cargo clippy --all-features`, `cargo fmt --all -- --check`, and
  `cargo run -p dist-helper -- check` passed. Distribution verification confirms
  the generated configuration schema and registry artifacts are current.
- Docker production acceptance passed remote reads and request filters, active
  versus disk policy divergence, explicit reload, lost receipt-body recovery,
  operation ownership and boot mismatch, legacy read-only authentication, and
  a real PTY successful reload with terminal restoration. The test-only partial
  fixture phase is still being debugged; full container acceptance remains open.

- The complete Docker flow subsequently passed, including the test-only HTTP
  partial fixture and both successful and partially applied dashboard PTY
  journeys. Partial CLI output preserves the operation report plus the CLI's
  existing JSON error envelope, exits nonzero, and identifies applied/failed
  participants. A separate synthetic-local-input trap phase is being added to
  close acceptance criterion 3 before the final container rerun.
- CI-equivalent `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features
  --no-deps` passed. All 35 relative links in the spec, ledger, docs index and
  shipped remote-administration guidance resolve.

- `cargo package --workspace --allow-dirty` passed: all eight packages were
  produced and their extracted sources compiled successfully (4m 33s). No
  publication or repository commit was performed.

- Final Docker acceptance after adding every CLI read and target isolation passed
  in 19.4 seconds. The rebuilt Linux/ARM64 image contains the wrong-token reload
  trap case. HTTP/CLI reads, filtered requests, staged policy divergence, scoped
  reload, receipt-body loss, same-ID recovery, boot changes, legacy credentials,
  and both PTY outcomes passed. Harness Bash syntax, Python compile, ShellCheck
  and diff checks passed; no temporary test containers or networks remain.
- Final `cargo nextest run --all-features --no-fail-fast`, including the added
  inventory-to-dashboard-page guard: **3,169 passed, 12 skipped**, 22.679 seconds.
  The unchanged SDK test `callbacks_and_unknown_extensions_are_bidirectional`
  received a non-failing nextest subprocess-leak annotation in that run; its
  focused rerun passed cleanly (run ID `447177c1-2869-4d14-b8a7-b414fd4dc199`).
  Formatting was rechecked after the test-only addition. This records the
  earlier specified acceptance scope; the expanded external-host MCP result
  below supersedes its former blanket Docker-acceptance status.
- Expanded live-interface verification added a standard-library MCP client that
  initializes the TLS `/mcp-control` endpoint, obtains runtime `tools/list`,
  requires the exact discovered control set, and semantically calls each tool.
  The default `tests/remote-administration/run.sh` now reaches that client after
  all CLI reads, then fails at `initialize` with `MCP HTTP 403: Forbidden: Host
  header is not allowed`. The production container logs record rmcp rejecting
  `NormalizedAuthority { host: "server", port: Some(8443) }`. Since the
  initialization handshake never completes, `tools/list` and calls to the
  expected `list_models`, `route_preview`, and `status` tools were not executed
  through the external TLS deployment. No Host rewrite or permissive bypass was
  used.
- The narrowly labeled diagnostic run
  `BITROUTER_REMOTE_ADMIN_SKIP_MCP=1 tests/remote-administration/run.sh` passed
  the independent Docker journeys. It executed an actual successful
  administrator CLI reload and retained `operations show` recovery, then both
  successful and partial PTY reload outcomes with terminal restoration and a
  usable subsequent shell. It is explicitly not a complete acceptance result.
- The successful production PTY journey rendered semantic Home, Models,
  Requests, Route, Providers, Telemetry, Agents, policy active/disk overview
  and named detail. It exercised route Backspace and Ctrl-U; policy Down then
  Up with typed-detail reset, PageDown then PageUp restoring the visible top,
  and the active/disk toggle. Agent Enter showed `Remote agent catalogs are
  read-only; start ACP sessions on the local host.` Conversation and Sessions
  remained remote-excluded with their existing generic unavailable messages;
  this is a copy/UX finding, not a claim of remote ACP execution.
- The request-filter harness now uses microsecond RFC3339 UTC bounds rather
  than a rounded current second. The passing selected run recorded CLI bounds
  `2026-09-08T07:52:02.640492+00:00` to
  `2026-09-08T08:02:02.645285+00:00`, with newest row
  `2026-09-08T08:02:01.963642884+00:00`; its HTTP bounds were
  `2026-09-08T07:52:07.129770+00:00` to
  `2026-09-08T08:02:07.134657+00:00` for the same newest row. This removes the
  prior same-second cutoff error without widening the assertion.
- Minor dashboard wording findings remain: Home calls the remote dashboard
  read-only even for an administrator with reload authority, and the inactive
  Conversation/Sessions views suggest choosing an agent despite remote ACP
  being unavailable. The live agent-launch attempt was correctly denied.
  This follow-up changed test assets and evidence only; production fixes remain
  separate from the live-testing results.


### Integration with main for PR review (2026-09-08)

- Merged `main` at `a59bd6b3` into the remote-administration branch. Preserved
  native transcript scrollback, the multiline composer, and docked controls.
  All eleven pages remain available; Policy and Reload use the available
  terminal height so typed details and retained operation receipts stay visible.
  Rendering tests exercise the actual dock height for policy and reload.
- Updated the PTY emulator for the main-screen writer's cursor-position query,
  bottom-row line-feed scrolling, erase ranges, and synchronized frames.
  PageDown/PageUp compares the complete policy body independently of automatic
  refresh timestamps, retaining the semantic scroll-and-restore assertion.
- Post-merge `cargo nextest run --all-features --no-fail-fast`: **3,195 passed,
  12 skipped**. An earlier run hit the existing timeout-sensitive
  `local_cli::tests::probes_compatible_old_failed_and_hung_workers` test; the
  complete final rerun passed, including that test. Documentation tests passed
  (five passed, one ignored), as did all-feature Clippy, formatting, strict
  workspace rustdoc, and distribution/schema checks.
- Rebuilt Linux/ARM64 selected Docker acceptance passed again: every remote
  CLI/REST read, target isolation, retained-operation recovery, policy controls,
  and both successful and partially applied real-PTY reload journeys. Both PTY
  runs restored terminal state and verified a subsequent shell write. This
  remains the explicitly MCP-skipped diagnostic selection; it does not close
  the external-host MCP limitation recorded above.
- Post-merge evidence: `/tmp/bitrouter-remote-merge-tests-final.log`,
  `/tmp/bitrouter-remote-merge-doctests.log`,
  `/tmp/bitrouter-remote-merge-clippy.log`,
  `/tmp/bitrouter-remote-merge-rustdoc.log`,
  `/tmp/bitrouter-remote-merge-dist.log`, and
  `/tmp/bitrouter-remote-merge-docker-verified.log`.
