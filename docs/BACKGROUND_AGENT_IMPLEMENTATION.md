# Background agent implementation ledger

Source contract: [BACKGROUND_AGENT_UX_SPEC.md](BACKGROUND_AGENT_UX_SPEC.md),
revision 3. Implementation authorized on 2026-09-20. The spec remains the
acceptance contract; this ledger records implementation and verification.

## Execution order and owners

1. **Supervisor foundation:** application-owned controller lifetime, local
   versioned transport, directory claims, permission policy, fenced leases,
   replay, minimal restart ledger, deterministic shutdown. Owner: supervisor
   implementor.
2. **CLI clients:** accepted background dispatch, session listing, stop/remove,
   standalone manager and attach; matching CLI help and shipped skill updates.
   Owner: CLI implementor.
3. **Presentation:** collapsed strip, bounded inline command center, separate
   drafts, permission focus, explicit alternate-screen history/detail inspector.
   Owner: TUI implementor. Presentation types can proceed independently;
   end-to-end acceptance depends on the supervisor foundation.
4. **Foreground migration and integration:** all Code sessions use the same
   supervisor; preserve route/settings/native-session controls, queue behavior,
   stop versus detach, terminal loss, and foreground-only scrollback. Owner:
   integration lead.
5. **Verification:** focused lifecycle, permission, replay and reducer tests;
   PTY interaction coverage; all-feature tests, Clippy, format, strict rustdoc,
   and diff checks; requirement-by-requirement audit against A1–A39.

## Contract boundaries

- New execution operations remain on the owner-scoped local control transport.
  Remote report contexts must not acquire local execution authority.
- The supervisor owns processes from creation. Clients own drafts and focus.
- Every mutation identifies its run, current lease generation and request ID.
- No background event is emitted into the foreground document journal.
- Optional native child activity remains unavailable unless negotiated through
  a typed capability; no parser or process discovery is introduced.

## Progress

- Spec and current session/daemon boundaries inspected.
- Three implementors dispatched using `gpt-5.6-sol` with `xhigh` effort.
- Supervisor, CLI clients, foreground migration, inline/alternate-screen
  presentation, and exact-scope local authorization are implemented in the
  working tree. Final integrated gates passed on 2026-09-21.
- The latest focused TUI run passed 240 tests and strict component Clippy,
  including complete 40×16 deck budgeting, cross-session permission focus,
  stale attach responses, retained stream coalescing/search, metadata
  attribution, and narrow target editing.
- Four focused real-PTY regressions passed: background history isolation,
  standalone attach/suspend/restoration, explicit foreground detach, and
  terminal loss followed by confirmed Stop during adapter initialization.
- The initialization Stop regression exposed a mutex guard held across a
  completion wait. The guard is now released before waiting; cancellation
  aborts the incomplete controller and waits for actual child reaping. Failed
  cleanup cannot release the directory claim or enable metadata removal.
- Intermediate workspace runs are not final evidence. The final gate record
  below supersedes earlier passing runs and fixture/render regressions.

## Final local gate record — 2026-09-21

All Cargo checks used `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`, and `CARGO_INCREMENTAL=0`.

| Gate | Result |
| --- | --- |
| `cargo nextest run --all-features --no-fail-fast --test-threads 6` | **3,519 passed, 0 failed, 22 existing opt-in tests skipped** across 33 binaries. Final same-source rerun ID: `7b5132ec-144f-400a-a205-317567ccb549`. |
| `cargo clippy --all-features --all-targets -- -D warnings` | Passed. No lint suppression added. |
| `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` | Passed. |
| `cargo test --workspace --all-features --doc` | 5 passed, 1 existing example ignored. |
| `cargo fmt -- --check` | Passed. |
| `git diff --check` | Passed. |

The 22 opt-in cases and one ignored documentation example were not executed;
this record does not claim live conformance with every external harness.
Existing toolchain notices remain for `proc-macro-error2` future compatibility
and macOS link-time unwind-table size; neither is a failed source check.

This is local implementation/verification evidence, not a CI or release claim.
At this local verification checkpoint, no commit, push, or pull request had
been created. Native child-agent visibility
remains capability-gated as specified; no heuristic discovery was added.

## Acceptance evidence

The real-PTY suite now covers accepted background dispatch surviving CLI exit,
foreground-only scrollback, explicit background history attachment, independent
foreground/background exit disposition, permission-safe signal handling,
standalone manager/attach suspend and restoration, permission defaults, timeout
while awaiting permission, schema validation before Ready, non-TTY output, and
foreground/worktree collision rejection.

The final gate record includes the latest startup-stop, failed-run cleanup,
authorization, native load/resume, and remote rejection cases. A passing
component test does not establish terminal behavior or daemon survival by
itself; the requirement mapping below distinguishes those evidence sources.

## Requirement traceability

The entries below identify implementation and automated evidence, not a claim
that a previous green run verifies subsequent edits. The final gate record
must cover the same tree as these references.

| Criteria | Implementation and verification |
| --- | --- |
| A1–A2 | Code uses the normal-buffer Writer; background events go only to the agent reducer. Real PTY checks ordinary entry and foreground-only scrollback before/after explicit background attachment. |
| A3–A4 | Code dock and key dispatcher retain F2/F3/F4 and add F5. Reducer/render tests cover collapsed and permission surfaces; PTY checks inline expansion. |
| A5, A10 | `expanded_agents_keep_the_entire_dock_within_forty_percent` renders 40×16 with multiline foreground draft, queued work, and a background permission. The complete deck is six rows or fewer. |
| A6–A7 | Snapshot reduction preserves focus/identity and compares only the displayed collapsed summary. `collapsed_summary_changes_only_for_meaningful_transitions` rejects stream-only repaint. |
| A8–A9 | Per-run reply editors and a separate foreground editor; target-bound reply, late lease, Unicode target editor, and PTY draft-preservation tests. |
| A11–A12 | No initial permission choice; exact permission/generation/request identity; foreground-priority gate at queue dispatch and History delivery. Tests cover buffered input, late lease acquisition, in-flight attach, and already-open inspectors. |
| A13 | Short permission/reply reducers remain inline; full structured tool context and long options escalate to a scrollable inspector. Tail-visibility tests prevent clipping. |
| A14–A15 | Explicit attach-only replay, sequence-gap marker, stream-coalesced search/copy/export, and retained client drafts/selection. PTY checks alternate-screen enter/leave without background scrollback leakage. |
| A16–A17 | All Code controllers use daemon-owned `SupervisedHandle`. PTY covers clean foreground stop, explicit detach, terminal loss, pending permission survival, and accepted background dispatch after CLI exit. |
| A18–A19 | Rows derive from typed supervisor snapshots, not process scans; settlement sets Idle + Unread (Ready for review), never task completion. |
| A20–A22 | Supervisor mutation gate, monotonic lease generations, transient release, idempotent action fingerprints, and exact offered permission options. Dispatcher tests reject stale generations including cached action replay. |
| A23–A24 | Detach releases control only; Stop confirms child reaping; Remove requires settled cleanup and never sends native session deletion. Failure cleanup acceptance test preserves failure/native identity. |
| A25–A26 | Per-run confirmed identity/route and source-labelled cost are projected in detail views; no aggregate token/budget metric is invented for the strip. |
| A27–A29 | Start records ownership before returning an attachable ID. Acceptance tests cover Ask versus explicit policy, permission-wait timeout, schema failure, and no-wait conflict; existing capability-led native load/resume tests remain enabled. |
| A30–A31 | Existing catalog subcommands retain their dispatch; clap compatibility tests and non-TTY manager tests assert escape-free structured output/errors. |
| A32 | Canonical Git worktree root claims precede launch; real foreground/background subdirectory collision test verifies the shared boundary. |
| A33 | Atomic snapshot/replay boundary, sequenced SDK notifications, exact prompt-response boundary, pinned pending actions/unread results, and duplicate/gap reducer tests. |
| A34 | Minimal ledger recovery maps live rows to Interrupted and marks history incomplete; no automatic native resume. |
| A35 | Native child discovery remains unavailable without typed negotiated semantics; no heuristic child-process inspection added. |
| A36 | Existing and new PTY coverage includes resize, CJK/emoji, paste, signal exit, suspend/resume, standalone manager/attach, and terminal restoration. |
| A37–A38 | Execution stays on owner-scoped local transport; remote contexts do not fall back to local execution. TUI owns presentation/effects only, with daemon/config/process/filesystem ownership in the application. |
| A39 | CLI reference, help, shipped skill and session reference updated together. Plugin manifests remain valid because they reference the unchanged skill distribution path and introduce no stale command surface. |

## Local authorization boundary

Spec §15.9 is separate from the single-writer lease. Owner-authenticated local
clients explicitly obtain capability grants scoped to start, list, peek,
transcript, attach, respond, stop, or remove. Metadata listing does not return
permission payloads or transcript events. Peek and transcript reads require
their own scopes; mutations additionally retain the existing run/generation
fence. Clients reuse grants without upgrading scopes after a denied request.

The first release trusts the daemon's OS owner to request these scopes over
its owner-only local socket; it is not a multi-user policy engine or a remote
execution authorization system. Grants are daemon-lifetime capabilities and
are not persisted in the run ledger. Their secrets are redacted in diagnostics.
