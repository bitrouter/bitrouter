# BRO six base tools: implementation and acceptance

Updated: 2026-10-01. Status: **implemented locally; macOS checks and real-model
E2E passed; Windows runtime tests and hosted CI passed. A test-wait robustness
follow-up is being checked separately.**
Contract: [six base tools v0.2](BRO_BASE_TOOLS_SPEC.md).

## Delivered behavior

Native coding requests declare exactly `read`, `glob`, `grep`, `write`, `edit`,
and `shell`. Read-only requests declare and permit only the first three. One
static registry owns declarations, argument validation, effect classification,
and dispatch. Legacy names have no aliases; unavailable and malformed calls
settle before approval or execution. Existing durable history is not rewritten.

`read` accepts UTF-8 files and immediate directory listings, including `.`.
Directory pages include hidden and ignored entries, sort by lowercase then
original name, quote names, and identify child symlinks without following them.
File lines and directory entries use one-based offsets. Complete records,
headers, and continuation messages fit within 50 KiB; an oversized single
record fails explicitly. Existing input, containment and special-file checks
remain enforced. `glob` preserves the former `find` matching/filter behavior.

Coding execution resolves Bash then POSIX sh on Unix, or pwsh then Windows
PowerShell on Windows, before model sampling. Declarations identify the resolved
executable/dialect. Spawn or command failure never retries under another
interpreter. Verification uses the same resolved executor and records its
identity. Read-only execution does not discover a shell. Unix dropped futures
now also kill the process group; timeout/cancellation/normal-exit cleanup stays
covered. Windows uses the existing job-object process wrapper.

The SDK adapters, CLI flags and external ACP tool names retain their contracts.
CLI/development docs, the shipped skill and its references, and all three
plugin manifests describe the six native tools. Boxing observed task events
keeps the existing wire JSON while satisfying strict clippy after verification
evidence grew.

## Local checks

Platform: macOS ARM64 (`aarch64-apple-darwin`).

| Check | Result |
| --- | --- |
| `cargo nextest run --all-features` | 3,602 passed; 22 skipped; final run reported no leaked tests |
| `cargo clippy --all-features --tests -- -D warnings` | Passed |
| `cargo fmt -- --check` | Passed |
| `cargo test --all-features --doc` | 5 passed; 1 ignored |
| Orchestrator unit tests | 36 passed, included in the full run |
| Separate-process CLI/daemon/SDK fixture | Passed; asserts six declarations and exercises all six handlers |
| Probe syntax, manifests and patch whitespace | Passed |

Tool tests cover profile enforcement, optional schema arguments in both OpenAI
adapters, legacy rejection, malformed numbers/properties, file/directory
pagination and byte bounds, case ties and invalid name conversion, path/symlink
escapes and special files, glob filters, availability fallback, fixed selection
after launch failure, nonzero/truncated output, and Unix descendant cleanup on
exit/timeout/cancellation/drop. The invalid filename conversion test exercises
the production helper directly because APFS rejects those filenames.

Local logs: `/tmp/bro-six-tools-nextest-final.log`,
`/tmp/bro-six-tools-clippy-final.log`, `/tmp/bro-six-tools-doctest.log`, and
`/tmp/bro-six-tools-unit.log`. Cargo also reports the dependency's existing
future-incompatibility notice for `proc-macro-error2`; no check was suppressed.

## Real-model E2E

The [reusable probe](../scripts/probe-bro-base-tools.py) starts an isolated
BRO daemon and database per run, creates a Python bug plus two tests, and routes
real inference through the existing local gateway to pinned
`bitrouter:openai/gpt-5.4-mini`. It records events and durable model declarations
and usage, checks actual files and independent verification, and stops its own
daemon. These are real model calls, separate from the deterministic HTTP fixture.

| Scenario | Observed result |
| --- | --- |
| Directed coding | Eight calls covering all six tools; two directory pages; code repaired and NOTES.md created; independent tests passed; zero tool errors |
| Read-only inspection | Six calls using only read/glob/grep; paginated directory inspection and correct bug diagnosis; all fixture hashes unchanged; no verification command |
| Natural coding, new interface | Eight calls; code repaired and NOTES.md created; independent tests passed; zero tool errors |
| Natural coding, old interface | Seven calls; code repaired and NOTES.md created; independent tests passed; zero tool errors |

The new model-issued `shell` and independent check both report `/bin/bash` and
dialect `bash`. Every durable request in each run declares the expected profile.
Provider-reported usage is present for every model step. This evidence proves
these controlled tasks completed; it does not establish Cloud charge settlement
or broad task quality.

The compact [evidence manifest](evidence/bro-base-tools/manifest.json) includes
binary SHA-256 values, task IDs, prompt hashes and full local artifact paths.
Each scenario directory preserves its summary, tool start/result/terminal
events, model declarations/usage, prompt, final fixture files and daemon cleanup.
Database and complete streamed events remain in the referenced local paths.

### Controlled comparison

Before source is the exact worktree snapshot
`55dad4a706905ac3cfef2b41e277daf8e0a69522`, including its existing runtime work.
On Unix it declares seven tools: read/ls/find/grep/write/edit/bash; the eight-name
registry filters out PowerShell. After declares six. Both natural runs use an
identical prompt and initial files and the same pinned upstream model.

| Metric | Before (7 declared) | After (6 declared) |
| --- | ---: | ---: |
| Independent tests | 2 passed | 2 passed |
| Model steps | 6 | 6 |
| Tool calls | 7 | 8 |
| Wall time, including isolated startup and check | 11.62 s | 8.98 s |
| First-step input tokens | 652 | 690 |
| Total reported input tokens | 5,920 | 6,710 |
| Total reported output tokens | 312 | 329 |
| Reported cache-read tokens | 1,152 | 2,304 |

This single pair supports functional equivalence on the fixture. The new run
was faster but used more calls and input tokens. Different model choices,
cache state, provider scheduling and startup timing prevent a causal efficiency
claim. Reducing tool names did not reduce the measured first-step prompt size.

An initial baseline build reused shared Cargo dependencies and incorrectly
produced six declarations. It was rejected as a comparison. The recorded before
run uses a rebuilt binary and confirms all seven declarations. The first provider
attempt through openai-codex failed authentication before any tool executed;
the successful runs use the already configured BitRouter route. Neither failed
attempt is counted as a successful E2E result.

## Reproduce and remaining gates

```sh
cargo build -p bitrouter --bin bro --all-features
python3 scripts/probe-bro-base-tools.py --binary target/debug/bro \
  --output-dir /tmp/bro6-review-coding \
  --upstream-model bitrouter:openai/gpt-5.4-mini --scenario coding
```

Use a fresh, short output directory and a configured inference gateway on
`127.0.0.1:4356`; supply `--api-base` for another unprotected gateway. Repeat with
`--scenario readonly` or `natural`. Each run consumes real model usage. Build
before/after in separate target directories, or force dependent source rebuilds
and verify actual declarations; shared cache reuse is not source identity proof.

- Windows runtime acceptance is proven by hosted Windows tests, including
  declarations, nonzero exit, capture caps/streaming and descendant cleanup. The
  earlier macOS cross-check failed in `aws-lc-sys` due to missing Windows SDK
  headers; that attempt remains excluded from successful evidence.
- macOS is the platform for real-model E2E. Windows CI uses deterministic model
  fixtures and actual PowerShell processes; no Windows real-provider E2E is
  claimed. Merge and release publication remain separate external gates.
- Future pending-call continuation/recovery belongs to runtime R4. Storage
  names/IDs remain untouched and new dispatch rejects old names, but this slice
  does not deliver restart recovery, continuous Threads, or the full runtime MVP.

All isolated test daemons were stopped. The existing gateway kept PID 65273;
its configuration and process were not restarted by these tests.

## Windows CI follow-up

A Windows-only test added after the recorded macOS live binary checks nonzero
exit status, stdout/stderr capture caps, interpreter identity and output arriving
before command exit. It changes tests only; the exercised production handlers
remain byte-equivalent to the live-tested implementation. The manifest's source
hashes identify the source at those live runs rather than the later test additions.
The local rerun passed 3,602 tests (22 skipped), strict clippy and formatting.
Logs are `/tmp/bro-six-tools-nextest-ci-prep.log` and
`/tmp/bro-six-tools-clippy-ci-prep.log`. [Draft PR #951](https://github.com/bitrouter/bitrouter/pull/951), stacked on
#945, runs the existing Windows matrix before the remaining cross-platform
gates can be checked. Its first [CI run](https://github.com/bitrouter/bitrouter/actions/runs/36820996682)
started against six-tool commit `9e9f2cf42b13075d7f32a1206a9e2dd3c237b878`.
The SDK rustdoc job found a redundant explicit Prompt link inherited from the
baseline. The follow-up removes only that link target; local workspace rustdoc
with `RUSTDOCFLAGS='-D warnings'` passed (`/tmp/bro-six-tools-rustdoc.log`).
The [corrected CI run](https://github.com/bitrouter/bitrouter/actions/runs/36821538609)
passed all 22 jobs at `964addb5d846274bcd0fb7f9086ba57f33c7b02a`: Linux/macOS/Windows
clippy, macOS and Windows all-feature tests, the complete split Linux test suite,
MSRV, rustdoc/doctests, SDK API/isolation, generated dist, formatting, repository
hygiene and the real plugin loader checks. [Compact CI evidence](evidence/bro-base-tools/ci.json)
preserves job identities and relevant Windows log lines. Windows ran 3,487 tests:
3,487 passed (one flaky), three skipped. All six-tool handler tests passed on the
first attempt, including all four descendant cleanup paths.

Both Windows runs reported the same verification fixture exceeding its three-second
observation wait and passing on retry. The follow-up gives this test helper ten
seconds on Windows; Unix retains three. Completion status, verification outcomes,
exit-code assertions and production timeouts retain their contracts. The local all-feature rerun passed 3,602 tests (22 skipped), strict clippy
and formatting. Final check logs are `/tmp/bro-six-tools-nextest-final-stable.log` and
`/tmp/bro-six-tools-clippy-final-stable.log`; the follow-up CI result is pending.

## Requirement audit

The six-tool contract is checked against production source, targeted tests,
separate-process fixtures, and the four preserved real-model runs. Current
production handlers reconstruct to the manifest's live-source hashes after
removing only the later Windows test and observation-wait adjustment. That comparison also confirms unchanged
agent/verification/process-fixture source.

| Contract requirement | Authoritative evidence |
| --- | --- |
| Six coding declarations / three read-only declarations | Registry profile test on each CI platform; every request in preserved real-model evidence uses its expected profile |
| Legacy/effectful rejection before approval or launch | Agent's allowed/validate checks precede approval; registry legacy rejection and read-only no-approval/no-mutation tests |
| Optional arguments and actual interpreter in SDK requests | Chat Completions/Responses adapter tests; process fixture captures real declarations; live shell/check results identify `/bin/bash` |
| File/directory behavior and pagination | Directory pages, empty/end pages, byte-bound/oversized-record, case/name/symlink tests; live coding and read-only pages |
| Containment and special-file rejection | Relative/canonical path validation and regular-file/directory dispatch; escape/symlink/socket tests; bounded complete-record tests |
| Glob rename preserves search semantics | Basename/relative-path/directory/ignore/cancellation tests; shared walker excludes child symlinks |
| Availability selection before sampling, no retry | Agent construction resolves once; ordered discovery and missing-after-selection tests; read-only constructs no interpreter |
| Bounded command output and cleanup | Unix output/exit/streaming and descendant tests; Windows output/exit/streaming and four descendant tests passed in Windows CI |
| Shared verification and history/approval identity | Verification clones the agent's selected tools; live result/check identities match. Store/context/history schemas and migration are unchanged from snapshot `55dad4a7`; legacy dispatch is unavailable before approvals |
| Required source checks | Local all-feature nextest/clippy/fmt/doctests plus strict rustdoc; hosted checks are tracked separately by commit |
| Shipped docs/skills/manifests | Six-tool CLI/skill references; all three manifest descriptions; hosted real plugin loader checks |

Pending old-call continuation remains a conditional requirement on future R4,
explicitly outside this slice. There is no restart continuation/replay command
in the delivered runtime. This change neither rewrites old names/IDs nor creates
aliases that can execute them or reuse their approvals. Cross-restart recovery
is not counted as delivered by this audit.
