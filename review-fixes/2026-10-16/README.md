# PR #981 review fixes: original evidence

This annex follows source checkpoint 8a44c6b1fc8a7d580de82412ab821e46c07c5d76. It retains the two independent review reports, exact patches for failed/passing local stages, original command logs, the verification runner and source hashes. Final qualified results are recorded in RESULTS.json after checks finish; earlier failed compilation and fixture attempts remain unqualified.

The scope is typed-decision cancellation/settlement, ACP recovery's existing protocol observation as a file-read barrier, complete daemon shutdown checks, narrow ModelControl identity storage, permission projection from already-checked material, and local PTY/IPC/presentation test setup. It does not alter CLI surfaces, retry allowances, execution authority, storage contracts or the read-only Workflow scope.

The cancellation regression uses actual GoalStore transitions and a controlled typed executor. It covers requirement/budget invalidation, owner transfer with the original unsettled hold, and input while paused. This is fixture evidence, not actual provider billing or vendor harness qualification.

The ACP peer updates its private file before publishing the restored prompt's permission request; the test waits for that existing observation before reading. The four real Goal PTY cases retain their original interaction assertions and additionally require a successful stop reply and process exit 0. These changes do not claim reproduction of a historical intermittent failure.

source-manifest.json binds 999 tracked executable, test, build and CI inputs to SHA-256. Documentation publication may differ from that manifest because it is outside those inputs. Current source requires the recorded public base; the recovery bundle records that prerequisite. Any copied fixture executable is a local Cargo-produced file, not a process/memory image or cross-platform qualification. No hosted temporary filesystem is claimed exported.

Intermediate compile attempts 01/02 retain original stderr and the available initial patch; no complete per-attempt frozen filesystem is claimed for those attempts. focused-03.patch and focused-04.patch were captured after formatting and before their respective runs. The qualified final inputs are additionally bound by source-manifest.json.

REVIEW_REPORT_PROVENANCE.json records the sole review-report publication omission: one internal memory-provenance paragraph. The local original remains intact; findings, source references and command logs are unchanged.

Final hosted qualification at d1c9074e9e6654fe77474571d7dc5e078a857c0a: 36 checks ended (29 success, 7 configured skips), no failure. hosted-ci/INDEX.json retains six workflow runs, 35 job records and 28 original raw job logs; SUMMARIES.json extracts actual test summaries and the new cancellation regression where observed. All earlier progress snapshots retain their original pending state. The attempted in-progress macOS log request returned BlobNotFound; it is diagnostic transport output, not a test failure or final job log.
