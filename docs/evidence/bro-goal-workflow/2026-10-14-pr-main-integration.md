# PR preparation and main integration

The user explicitly requested a PR targeting main on 2026-10-14. This
authorizes committing, pushing and opening this PR. The implementation Goal
remains unfinished, so the PR is a draft; no merge or release is authorized.

Main advanced from 9b9a1762 to 9956a427 with the native BRO ACP server and SDK
ACP migration. The feature commit was rebased onto that main commit. Conflict
resolution preserves Goals for bare bro/code, native BRO ACP for bare acp serve,
and transparent external acp serve with an agent argument. New Goal imports
use orchestrator ACP and SDK config::agent. Skill/plugin metadata and the
development guide preserve both entrypoint contracts. The handoff build
fingerprint now includes the resource/recovery implementation modules.

Historical raw diagnostic/test outputs remain in the evidence directory. For
Git whitespace hygiene, 35 log/text files had trailing whitespace and extra
blank EOF lines normalized. Original byte-for-byte copies were preserved in a
private local temporary directory before normalization. Test outcomes and
source SHA manifests were not changed. The published logs are normalized
copies, and are not claimed to be their original byte streams.

The previous 4,564-pass result applies to the pre-rebase source. Integrated
source verification and hosted CI are recorded separately. Real harness,
provider, typed-backend/input-quality and remaining G/A/F gates are incomplete.

Draft PR #981 targets main at feature head b53ae1c2. The first integrated
workspace run exited 110: the Cargo-built test peer could not be built because
the disk filled (os error 28), then nextest could not write its result report.
This is not a completed test run. The original partial/failure log is retained.
Owned test/Cargo/fixture processes had exited before cleanup. The explicitly
checked worktree target was Git-ignored and had no symlinked deps directory.
Package-only cleanup dry-run listed 3,127 files/20.0 GiB; actual cleanup removed
3,096 regenerable bitrouter artifacts/19.0 GiB, leaving about 19 GiB free.
Sources, evidence, dependency caches and other worktrees were preserved. All
102 source hashes matched again. A second full run uses the same source,
features, debug/incremental settings, test concurrency and normal deadlines.

The second full run `6096b54b-1143-4db1-b281-263afe63e78b` passed all 4,580
executed tests across 71 binaries, exit 0, 688.326 seconds, nine slow cases,
22 existing opt-in skips and no LEAK or timeout. Combined session 93837 then
passed strict workspace/all-target/all-feature Clippy (1 minute 11 seconds),
separate doctests (six passed, one existing ignored example, seven suites),
formatting, full-branch diff checks and all 102 source hashes. Final inventory
found no owned test/Cargo/rustc or ACP/PTY fixture processes remaining.

These checks identify code head b53ae1c2. The following documentation/evidence
commit does not alter code. Hosted CI at that head had 25 successful checks
and macOS/Windows test jobs still running; it was not reported as all green.
The PR stays draft and unmerged because the broader MVP gates remain open.
