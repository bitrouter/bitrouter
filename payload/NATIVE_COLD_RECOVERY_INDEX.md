# Native Goal checkpoint continuation

Parent 9c5a0ea1. The full refactor remains active and PR #981 remains unpushed. Native Goal observation can reconnect its current live control/accounting gate to the original accepted Thread/Turn through existing shared recovery. Original action/body, source, stopped-owner, effect and budget guards remain authoritative. No new store or schema.

## Evidence

- red1 reproduces a safe accepted native checkpoint only being projected as unknown.
- green2 failed because the direct-service test omitted workspace registration; green3 failed because a new accounting assertion was placed before continuation in the paused case. Both fixture errors and their original logs remain, without changing product permission/budget rules.
- Each failed run has an independently extracted and hash/mode-checked 22-file SQLite/artifact image. Successful temporary cases are cleaned; these archives are filesystem checkpoints, not process images or machine-wide effects snapshots.
- green4 passes five internal boundary cases, 82 Goal runtime tests, 10 shared worker tests, strict workspace/all-target/all-feature Clippy and fmt. Cases cover safe settled read, live source owner, pending read result, prior pause and pause after entry authorization. Changed retry body is refused before restoration, the confirmed read is not replayed, original Thread/Turn and cumulative Goal accounting survive.
- follow5 is byte-identical source: 43 recovery/generation/native tests and five matrix stress iterations pass. Both runs have no source drift or nextest LEAK marker. They do not establish or fix the cause of earlier output-handle reports.
- Continuation uses a controlled SQLite checkpoint image made while a real ACK barrier is held; the actual source is then shut down and joined before the same owner is retired in the image. This does not claim abrupt crash takeover, universal normal-restart continuation or external ACP private-state recovery. HTTP/model responses are fixtures; native SDK requests and native read execution are real.

All five source tars and the final source tar were independently extracted and checked. After source4/5 only the declared documentation/plugin descriptions and one exact module-comment line changed. See native-cold-applicability6.json and native-cold-manifest6.json. ACP same-session recovery, worker quote waits, historical evidence closure, live qualification and final full-source/hosted-CI/publication gates remain open.
