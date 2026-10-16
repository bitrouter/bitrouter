# Native worker budget admission checkpoint

Parent: `97b02928b9d31b0db9a8470f37be2e02eda45f2a`. The full refactor remains active; this local checkpoint does not push PR #981 or claim a current full-workspace pass.

## Behavior

Native workers retain the original prepared plan and exact frozen quote. A quote blocked only by other current workers' in-flight reservations waits under the same action key and live source/lease/budget checks. ResourceWait facts expose this phase without provider-dispatch claims. Known spend, settled missing-usage holds, revoked sources and hard limits remain fences. The SDK races admission against cancellation and records a no-dispatch abort only before polling that attempt's executor; earlier calls or external charges are not zeroed. These waits do not cover owner/input generations or private ACP costs.

After the elapsed cap is durably recorded, the coordinator avoids repeated identical halt observations. Admission continues to calculate age from original creation, and raising the limit still requires explicit resume. Production CAS and UI outbox semantics are unchanged. The editor test permits one explicit user Enter after a definite rejection and a matching refreshed snapshot, then requires exactly one stored Input with identical original bytes. It never resubmits uncertain acceptance or increases the deadline.

Goal IPC is 8 with writer fence 000033; Runtime 9, native 17 and HTTP 2 are unchanged. CLI references, three plugin descriptions and the 199-line shipped skill match.

## Results and original failures

- red1 reproduces transient worker quote contention being refused. green2 passes initial accounting/SDK boundaries and strict checks; its source predates later refinements.
- final3 passes five boundaries and 4,749 full-workspace tests, with one failure, 24 skipped and no LEAK markers. RIGHT actual startup crossed LEFT's fixture release clock; its 116-file failed image is independently extracted and hash/mode verified. A subsequent fixture barrier starts LEFT's separate release clock only after RIGHT actually starts. Parent deadlines and business retry limits are unchanged.
- final4 passes five worker-switch stress iterations and 4,749 full-workspace tests, with one failure, 24 skipped and no LEAK markers. A PTY peer transport closed at initialize; its cause is unconfirmed. That old fixture did not retain its filesystem on this error, so only source/raw logs exist for this run.
- diag5 passes 20 PTY startup/review iterations, strict Clippy, fmt and six doctests, with no source drift or LEAK markers. Passing repeats do not establish the earlier closure's cause. New PID-specific diagnostics and failure retention change neither ACP stdout nor process semantics.
- final6 passes five boundaries and 4,748 full-workspace tests, with two failures, 24 skipped and no LEAK markers. One was the old budget-label expectation. The other assumed a fixed historical attempt count despite a confirmed pre-session initialization failure. Preserved protocol records show LEFT two actual prompts, RIGHT one and JOIN one. Its 349-file panic-retained filesystem image is independently verified; its in-memory database was not captured. The repaired assertion compares the actual running source and unchanged RIGHT Thread/Turn, and a deliberate first-initialize failure tests that combination.
- final7 passes five boundaries, four repair/TUI cases and 4,749 full-workspace tests, with one editor CAS failure, 24 skipped, seven slow and no LEAK markers. Its 15-file image includes SQLite and recovery drafts. Database history proves repeated elapsed observations after a halt; the rejected request body is absent, so it does not identify the exact competing update.
- red8 deterministically reproduces redundant elapsed writes with a logical past checkpoint and real SQLite (revision 3 to 4). Its three-file image is independently verified. No provider or harness is involved in that clock regression.
- green9 passes 87 Goal/component cases, 735 SDK cases and four Goal PTY cases. Its first editor stress iteration still sees a definite initialization CAS conflict; its 17-file image is independently verified. This proves that removing elapsed churn alone does not eliminate all version competition.
- green10 passes four Goal PTY cases, ten editor stress iterations, strict workspace/all-target/all-feature Clippy, fmt and six doctests (one ignored). No source drift or LEAK markers. Only the editor test and implementation ledger differ from green9; the audit verifies identical production bytes for the 87/735 component results. No fresh full-workspace pass is claimed for this final production source.

Later jobs after failed runner stages did not run. Every campaign's source, config, runner, raw output, terminal result and drift audit remains separate; these sources are not interchangeable.

## Other diagnostics and reproducibility

The first standalone PTY probe used an overlong 108-byte macOS socket path and failed because of its own layout. Its source and error remain. The corrected short-path probe records healthy initialize/EOF (exit 0, one protocol response) and a deliberate missing-parent bind failure (exit 1, no protocol response); both owned processes exit. These probes verify diagnostics, not a real harness or model. Fixture-specific macOS logs also contain AMFI warnings during passing runs; both recorded fixture binaries pass strict signature verification. No system security setting was changed, and these records do not establish the initialization closures' cause.

`token-wait-applicability11.json` verifies extraction of all ten source tars, the final dirty-source tar and the exact post-test documentation delta. `token-wait-manifest11.json` hashes campaign records, filesystem archives and probes. Failed filesystem copies are not process images, complete private ACP state, machine-wide effects snapshots or arbitrary crash-recovery proof. Successful temporary fixtures are cleaned normally.

The archive is currently local and separate from the feature PR. A verified Git bundle/checkpoint record will bind the resulting commit to this manifest. Final S0–S6/A01–A22 audit, historical/native recovery closure, ACP same-session qualification, real harness/provider limits, fresh final full checks, sanitized independent publication, PR update and hosted CI remain open.
