# Shared generation/native admission evidence

Local checkpoint: d2c5ad519c438d655b3ac43786c8e4a832d4f9b8. It has not been pushed to PR #981.
The refactor is incomplete: S3-S6 and the final A01-A22 audit remain open.

Latest source: shared-admission-source2.json; commit binding: shared-admission-local-checkpoint.json.
Full gates: shared-admission-gates2.json. All four commands exit 0. Nextest: 4,725 passed, 24 skipped, one leaky exit; no failed or aborted cases. Rustdoc: six passed, one ignored. This is not a leak-free or final acceptance claim.
The leaky full-run case is typed_quotes_require_bounds_for_every_nonzero_price. Focused run 5 passes sixteen cases and marks a different cancelled-decision settlement case leaky. Their causes are not established.

History: focused1 has two compiler errors before the first complete source manifest; focused2/3 pass nine/ten service cases. Full1 has 4,721 passes, four native Goal stack overflows and 24 skips. Focused4 is blocked at six app loopback binds by sandbox policy; its ten service cases pass. Focused5 uses local-port permission and passes all sixteen cases. No stack-size, timeout or assertion relaxation is used.
The old README.md and earlier summary files describe their own historical gate sets and are preserved unchanged; use this versioned index for this batch.

Failed fixtures: shared-admission-stack-overflow-fixtures1.tar.gz. Four surviving filesystem fixtures, 740 regular files; four sockets explicitly omitted. Extraction and all member hashes were verified. SQLite was in memory and process/backend state is unavailable; exact case-to-directory mapping is unconfirmed. Original fixture directories are unchanged.
Failed source: shared-admission-unboxed-source1.rs, reconstructed by reversing only the boxing correction and SHA-256 matched to source1. Provenance is explicit in shared-admission-source-lineage.json. All other executable/build files match source2. This is not represented as an original raw source capture.

Reproduce the current local checkpoint:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo nextest run --workspace --all-features --no-fail-fast
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo test --workspace --all-features --doc --jobs=2
```

Use loopback-port permission for the real mock-server/IPC fixtures. Cargo-built protocol fixtures remain in helpers/bitrouter-test-fixtures; no Python fixture is required.
Raw logs and intermediate evidence stay outside the feature PR. This archive is currently local; it has not been published to the independent Git archive. The existing pinned PR archive is unchanged.
