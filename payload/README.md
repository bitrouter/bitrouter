# BRO refactor local verification

Captured after the user rebooted macOS on 2026-10-16. See verification-summary.json for outcomes and SHA-256 file identities. Source identities are in after-reboot-full1-source.json; the source drift audit is in after-reboot-source-audit1.json.

Reproduce from the recorded workspace source with:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo nextest run --workspace --all-features --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo test --workspace --all-features --doc --jobs=2
```

The full run passed 4,720 tests and skipped 24. Rustdoc passed six with one ignored. All four processes exited 0. The first affected run failed one test because its mock supplied one response for two model calls; the corrected fixture passed all twenty affected cases without weakening the completion assertion.

Pre-reboot /private/tmp evidence no longer exists. This directory does not reconstruct it. Green local regression does not qualify pending ACP integration, live providers/harnesses, hosted CI or remaining spec acceptance.

## Latest local gate set

The final restored source is in late-stop-source6.json, with the final drift audit in late-stop-source-audit-final.json. late-stop-gates3.json records all four exit codes as 0. The full run passed 4,722 tests, skipped 24, and reported no failed, timed-out or leaky cases in 562.452 seconds. Strict Clippy and format pass; rustdoc passes six with one ignored. See late-stop-verification-summary.json for SHA-256 identities.

Earlier full runs reproduced an editor Enter-persistence failure and a route-startup error assertion failure. The stronger editor fixture alone did not fix the former. A nonblocking-reader experiment failed all four PTY cases and was reverted byte for byte; that code is absent from final source. The restored source passed 24 focused cases and five two-case stress iterations, followed by the green final full run. Root causes of the earlier intermittent failures remain unresolved and recorded as S6 work. One earlier focused run also reported a separate active-Goal case leaky. No standalone repetition replaces the failed full-run evidence.

Gate set 2 was deliberately interrupted after two actual failures: 4,716 passed, two actually failed, one was interrupted, three were not run and 24 were skipped. Its later checks were not run. Gate set 1 completed 4,721 passes, one failure and 24 skips, with its later static/format/rustdoc gates passing. Both records and all output remain here.

Managed ACP migration and S3-S6/final acceptance remain incomplete. No commit, PR update, hosted CI, live provider or real-harness qualification is claimed by these local results.
