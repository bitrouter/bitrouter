# Final review fix report

## Scope

This fix wave addresses only the final-review findings requested after
`201880934105307b81e49b14a4fe2629c4ae6d70`:

- Important: share one credential manager across standalone ACP routing and
  account telemetry.
- Minor A: retain `TempDir` guards in hosted-applier and Cloud-glue tests.
- Minor B: make raw `Authorization` precedence prove that OAuth work is not
  performed.
- Minor C: correct the account-store ownership comment.

## Changes and evidence

### Standalone ACP manager sharing

`StandaloneCloudCredentials` creates one best-effort default
`Arc<CredentialManager>` at each `serve`, `chat`, and `prompt` invocation.
The same holder is threaded into routing fallback and into the internal
standalone exporter builder. The public exporter builder retains its original
single-argument surface for non-ACP callers.

The new ACP wiring regression uses a near-expiry OAuth fixture and the actual
standalone holder's routing and telemetry paths concurrently. It observes the
same rotated bearer on both paths and exactly one metadata request plus one
refresh request, so independent managers cannot satisfy it.

TDD evidence:

- RED: before the holder existed, the focused test failed to compile with
  `unresolved import super::StandaloneCloudCredentials`.
- GREEN: `cargo test -p bitrouter acp_cli --lib` passed 7 tests, including
  `standalone_wiring_single_flights_routing_and_account_telemetry`.

### TempDir cleanup

Both test path helpers now return `(TempDir, PathBuf)` and each call site keeps
the guard alive until test completion. `rg 'keep\\('` over the two requested
files returns no matches. Hosted applier tests passed 12/12 and Cloud glue
tests passed as part of the 63 Cloud tests.

### Raw Authorization precedence

The raw-header test now stores a valid near-expiry OAuth credential, so the
constructor still reads the login origin. It asserts successful request
delivery with the caller's final `Authorization` header, preserved repeated
headers, and zero metadata and refresh requests.

TDD/mutation evidence: changing the production condition to resolve OAuth
when a raw header is present made the focused test fail with the expected
metadata 404. The condition was restored; the focused test then passed.

### Store comment

The `build_auth_appliers` comment now accurately says that the account store
and generic provider store are isolated modules in `bitrouter-providers`.

## Changed files

- `apps/bitrouter/src/acp_cli.rs`
- `apps/bitrouter/src/assemble.rs`
- `apps/bitrouter/src/cloud/api_client.rs`
- `apps/bitrouter/src/cloud/mod.rs`
- `crates/bitrouter-providers/src/hosted/applier.rs`
- `.superpowers/sdd/2026-08-31-cloud-sdk-consolidation/final-fix-report.md`

## Verification

All wire tests below clear `http_proxy`, `https_proxy`, `all_proxy`,
`HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`.

- `cargo test -p bitrouter acp_cli --lib` — 7 passed.
- `cargo test -p bitrouter cloud:: --lib` — 63 passed.
- `cargo test -p bitrouter-providers --features hosted hosted::applier --lib`
  — 12 passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  passed.
- `cargo fmt --all -- --check` — passed.
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`
  — passed.
- `cargo nextest run --all-features` — reproduces the accepted baseline
  residual: 218 passed, 1 failed,
  `continuation::tests::legacy_ciphertext_resolves_without_guessing_an_effective_model`,
  at unchanged `apps/bitrouter/src/continuation.rs:3312`; nextest cancelled
  the remaining tests after that failure. The same residual is recorded in
  `progress.md` and is outside this diff.

## Self-review

- Confirmed no public sibling re-exports were introduced.
- Confirmed the changed production code contains no new `unwrap`, `expect`,
  `panic!`, or `#[allow]` patterns.
- Confirmed standalone non-telemetry and unavailable-manager paths remain
  best-effort (`None`) as before.
- Confirmed `git diff --check` is clean.
