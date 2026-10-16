# Managed ACP shared-runtime evidence

Captured 2026-10-16 for PR #981, on local parent checkpoint
`d2c5ad519c438d655b3ac43786c8e4a832d4f9b8` plus exact saved diffs and untracked files.
These are local Rust/HTTP/protocol fixtures. They do not qualify hosted CI,
credentialed harnesses/providers, production or complete S3–S6 acceptance.

| Capture | Result and source applicability |
| --- | --- |
| `managed-acp-focused1.log` | 17 passes, 9 stack-overflow aborts. Source changed during compilation; not source-qualified. No complete source1 manifest was captured. |
| `managed-acp-focused2.json` | 26 passes, no source changes. Bound to `managed-acp-source2.json`, patch and source-files tar. |
| `managed-acp-focused3.json` | 74 cases: 53 passes, 21 stack-overflow aborts, no source changes. Original source3 patch and source-files tar retained. |
| `managed-acp-focused4.json` | 74 cases: 73 passes, one native stop/projection assertion failure, no stack-overflow aborts and no source changes. Bound to source4. |
| `managed-acp-clippy5.json` | Strict Clippy rejects one collapsible conditional. Bound to source5, including the stop/projection source check. |
| `managed-acp-gates6.json` | Authoritative state for the source6 strict lint, format, full workspace and rustdoc run. Read statuses and final source audit; a running or failed command is not a passing gate. |

`managed-acp-check1.log` through `check5.log` and `test-build1/2.log` retain
intermediate compile diagnostics. They are not substitutes for the final gates.
Each versioned source manifest contains per-file SHA-256 and the parent Git HEAD;
patches retain tracked changes and source-files tars include untracked sources.
The archived Node runners record exact arguments and non-secret build profiles.
They use the normal stack limit and unchanged test bounds.

## Stack diagnosis and crash fixtures

`managed-acp-frame-and-fixture-evidence4.json` binds the temporary diagnostic
probe and its outputs. Source3 plus that archived test probe measured futures
of 32,880 bytes (supervisor dispatch), 38,384 (Goal task), and 40,720 (Goal worker).
Boxing the awaited startup and receipt-query boundaries reduced them to 8,912,
16,888 and 18,304 respectively. The diagnostic probe is not product test code.
The debugger attempt remained suspended before test output; its three verified
processes were ended and it produced no usable backtrace.

`managed-acp-stack-overflow-fixtures3.tar.gz` preserves 17 surviving fixture
directories and 2,103 regular files. SHA-256:
`3f871e4e1c62376abcbe8e52f470b5dbb762a408906b60e67837c9653d2cb91c`.
An independent extraction verified all 2,103 payload checksums against the
embedded manifest. Fifteen sockets, symlinks or other non-regular entries were
recorded but not copied. Exact failed-case to directory mapping is unconfirmed;
in-memory SQLite, process and provider state cannot be reconstructed. Original
surviving temporary directories were not changed or deleted.

## Remaining qualification

Warm shared admission, real session mapping, original permission replies and
owned stop are distinct from confirmed same-session cold continuation, late
stop/effect reconciliation, complete interruption coverage and safe worker
replacement. Native resource/activity evidence, native parallel/dependency
closure, legacy-loop removal, resource waiting, live qualification and final
A01–A22 remain separate work. A green regression checkpoint does not complete
the refactor. Raw material remains outside the feature PR; publish its final
traceable archive separately after completing the authorized implementation.

## Source6 final gate result

All four gates passed with zero file changes during the run. Full nextest: 4,730 passed, 24 skipped, seven slow, zero failures/aborts/leak reports (563.544 seconds). Rustdoc: six passed, one ignored. Strict Clippy and format passed. The local checkpoint is `9d0460696b39b12b8585334b401224fbafde6494`; only three Markdown files changed after the gates, with their exact bytes/hashes retained separately. `managed-acp-local-checkpoint6.json` binds it to source6 and the verified Git bundle. It has not been pushed. The fixture archive preserves file contents/checksums; original modes were not captured by the forensic copy. Final acceptance and the independent published archive remain open.
