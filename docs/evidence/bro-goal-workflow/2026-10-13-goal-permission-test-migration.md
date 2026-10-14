# Goal permission test migration

The active Goal PTY now has two simultaneously pending actual ACP permission
requests. The Cargo-built peer uses distinct RPC IDs, tool IDs and request
materials on the same execution. Neither write nor confirmation runs until
both explicitly allowed responses reach the peer. It then writes and reads the
actual file and reports both tool completions.

| Retired UI test | Replacement evidence and semantic change |
| --- | --- |
| `permission_arriving_in_inspector_needs_slash_and_fresh_selection` | Both requests arrive while the user is reading fullscreen Workflow detail. Numbers, Enter and Ctrl-A in that view leave both Pending and no file effect. Explicit return to current Goal and Ctrl-A opens a pinned card. First and second selections remain distinct, each response is delivered exactly once, and the peer confirms receipt. Both workers remain active while Goal input is processed. Retained native session API tests still cover overlapping raw protocol permissions. |
| `code_minimum_and_below_minimum_permission_paths_are_safe` | At 40x12, both offered choices remain visible; choosing then Tab cannot transfer selection to the second request. At 30x10, Down/numbers/Enter/control keys leave both Pending. Explicit redraw acknowledges input consumption before resizing. Restoring the viewport or reopening a card cannot reuse a hidden selection. The new contract uses Esc as back without responding; the old Code view's implicit Esc denial is replaced deliberately. Explicit denial remains independently tested through state and native permission APIs. |

## Product correction found by this test

The first run delivered one response to the actual peer but a quick reopen
could use the client's cached first request. The pinned dialog did not react to
new snapshots, so it remained on that already answered request. Runtime still
rejected duplicate responses; the UI did not provide usable progression.

`State.receive` now keeps monotonically increasing Goal revisions and compares
the pinned request with the current pending set. Resolution or any binding
change invalidates the card. It stays focused and inert: old Enter, Tab,
selection keys, retries and paste cannot respond or submit the hidden draft.
Late evidence cannot restore it. The footer explains that the request is no
longer pending; Esc followed by explicit Ctrl-A opens the current remaining
request with no selection and no reused material. There is no automatic
retargeting to a different request.

The existing valid-pinning tests retain the same offered request across a newer
snapshot. New tests cover resolved requests, late old snapshots, replaced
materials/options, draft preservation and explicit reopening.

## Observed runs

- V1: run `59e868a0-6bab-4e95-aef9-206a08eaa285`, 0 passed, 1 failed, exit 100,
  99.066s. Actual first approval reached the peer; second-card waiting failed.
- Initial stale-card unit slice: 21 passed, exit 0, 0.031s. Later source also
  ignores stale evidence and tests a changed binding; it requires fresh checks.
- V2: run `1ad5076b-9d60-42f5-95c8-f684f77cab56`, 0 passed, 1 failed, exit 100,
  115.103s. Second permission remained Pending. A label alone was insufficient
  evidence that its actual request material had loaded and verified.
- V3: run `afb65d15-1764-430e-9789-926da923d589`, 1 passed, exit 0, 91.869s.
  Before the second explicit selection, the test waits for the actual
  `Confirm right.txt` material. It then observes both distinct delivered
  responses, actual file effects, detach, dependency handoff and verified
  delivery. Original deadlines and assertions remain unchanged.

V3 passed before retirement of the two obsolete entry tests and their unused
scenario/wrapper. Their final source, broader regression, state tests and
static gates are recorded in the implementation ledger. The old fixture's
unused inspector marker is removed; generic permission control and all
remaining protocol/API scenarios stay available.

The first final regression reported 51 passed and 1 failed, run
`51a7c312-a600-4820-8b80-2e930a0d0709`, exit 100, 107.004s. The test matched
permission words in the whole screen, including history, and sent adjacent
Esc/Ctrl-A bytes that the pinned terminal parser treats as an Alt combination.
It now identifies the live footer, sends Esc alone, awaits a fresh complete
composer frame, and sends Ctrl-A separately. This acknowledges the actual
focus transition without retrying an authorization or widening deadlines.

Final v2: app regression 52 passed, run
`62f55ed4-e906-4d1f-b49f-e3e0a1092d29`, 103.305s, exit 0; state regression
22 passed, run `bee75914-1b32-4d86-99ce-94e715823287`, 0.037s, exit 0.
Workspace/all-target/all-feature Clippy with warnings denied, formatting,
diff and 98-file identity checks passed. Separate workspace doctests:
6 passed, 1 ignored, exit 0. No owned test processes remain. All failed runs
are retained. Six root legacy UI scenarios and opt-in native/evolution old
paths still require migration, so complete workspace/MVP acceptance is pending.

These are real local CLI/PTY/ACP/runtime effects with fixed provider judgments.
They do not establish real-model understanding, all harnesses, cross-platform
execution or complete MVP acceptance.
