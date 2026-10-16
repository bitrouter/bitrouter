# Worker replacement and requested-worker provenance evidence

Local PR #981 checkpoint based on 9d0460696b39b12b8585334b401224fbafde6494.
This is partial S3 work. Complete refactor, live-provider/harness qualification,
hosted CI and final publication remain separate gates.

Each source manifest records all repository file hashes and its parent HEAD.
The matching binary patch and source-files tar retain the dirty source,
including untracked Rust modules. Runners record exact commands and unchanged
normal stack/time bounds. Logs include compile failures and failed assertions.

| Source | Observed result |
| --- | --- |
| 1 | Compile rejected: fixture helper attempted to serialize Config. No tests ran. |
| 2 | Compile rejected: new fixture input arguments were reversed. No tests ran. |
| 3 | 113 passed, three failed: legacy archive key, domain revision, hot archive length assertion. |
| 4 | 115 passed, one domain revision failure; actual ACP-to-ACP and ACP-to-native result transfer passed. |
| 5 | 115 passed, same domain revision failure; no source changes during run. |
| 6 | Isolated domain test failed at stopped-effect reconciliation; phase diagnostics retained. |
| 7 | Correct control revision exposed missing replacement workspace registration. |
| 8 | Corrected domain test passed; all source hashes stable. |
| 9 | Full required checks: consult worker-switch-gates9.json and source-audit9.json. Running/failed is not a passing gate. |

The domain fixture now uses its actual saved control source and registers the
replacement workspace before testing resource admission. Runtime version,
effect, workspace and retry guards were not weakened. Missing/foreign transfer
material is rejected, while preparation-only retries retain original results.

The actual ACP replacement peer checks inherited LEFT bytes before session
startup. The native replacement reads inherited LEFT with the native read tool
before producing a result. Changed old-worker provenance cannot relabel these
original run/session bindings. Unknown old tool effects block replacement;
a worker edit never resumes an explicit pause. Independent right work is retained.
These are deterministic Rust/HTTP/protocol fixtures, not real-model quality proof.

## Artifact applicability and retained fixtures

worker-switch-trial-index.json binds trial metadata and checksums. Runner1
accidentally copied earlier managed-ACP check1..4 and test-build1/2 logs under
worker-switch names. They are retained and explicitly mapped to their byte-
identical managed-ACP originals; they do not verify this worker-switch batch.
worker-switch-initial-check1.log is the actual early worker-switch check, which
predates later handoff implementation and is not final-source qualification.

Necessary Cargo-built fixture source and regression tests remain in the feature
PR. Their exact intermediate bytes are also in the source tars/patches; the
Git checkpoint/bundle identifies the unchanged fixture package and dependencies.
Normal tests clean their temporary runtime directories and in-memory databases;
no claim is made that those transient runtime states were captured for these
trials. Original failed logs and all source snapshots remain available.
Earlier managed-ACP stack-abort crash fixtures have their own independent
archive and checksum audit; this batch does not rewrite or relabel them.

Raw evidence currently resides in this local archive. Preserve it in a separate,
sanitized, commit-pinned archive for final publication. The functional PR should
retain final acceptance/reproduction material, not the raw campaign snapshots.

## Final local checkpoint

Local commit 867642d95bc5a74e8b777bce2d3bb3f79f11a8a5 is clean and remains unpublished. All four source9 commands passed: strict Clippy, format, 4,735 tests (24 skips, six slow, no failures/aborts/leak reports), and six passed/one ignored doctest. Its zero-drift aggregate correctly remains false because a preceding formatter wrapped one test expression during initial snapshot capture. The exact token-preserving delta and both source manifests are retained in worker-switch-source9-applicability10.json. Source10 then passes all 116 targeted cases with zero changed files. Both source tars independently verify all 41 saved files. Only the implementation Markdown changed after those checks; its exact bytes/hash are in worker-switch-precommit10.json. The verified Git bundle retains all three refactor checkpoints from the original PR head. This checkpoint does not complete or publish the refactor.
