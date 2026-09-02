# ACP controller Phase 3 execution plan

Status: **implementation complete; PR validation pending** · Date: 2026-09-02

This plan is derived from [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md).
It records the already-delivered Phase 2 baseline without pretending to be a
historical pre-implementation artifact, then defines the executable Phase 3
work and completion gates.

## Phase 2 completion ledger

Phase 2 was implemented and merged by
[`feat(acp): add controller identity and session routing` (#852)](https://github.com/bitrouter/bitrouter/pull/852)
at commit `d3920084`. The merged change supplied the implementation plan through
the Phase 2 sections of the controller spec and delivered:

- controller and native Claude/Codex request identity;
- in-memory session route leases and `_bitrouter/route/*`;
- controller, session, trace, OpenTelemetry, and metering correlation;
- provider setup and native ACP lifecycle contract coverage; and
- workspace verification on Linux, macOS, and Windows.

The PR reports 2,891 passing nextest tests, clippy, formatting, rustdoc, and a
final Critical/Important review with no remaining findings. A Phase 3 audit of
the merged design found one intentional correction: the `brac_*` credential
does not attest caller-declared controller or session headers and duplicates
the existing API-key boundary. The current spec supersedes it. No separate
Phase 2 implementation will be created.

## Phase 3 outcome

The controller remains an ACP v1 harness process. Model calls use the normal
BitRouter API key, or the deliberately open local principal under
`skip_auth`. Route state is ephemeral and keyed by the same API principal plus
declared controller/session claims. The controller forwards the stable v1
surface supported by its pinned Rust SDK without taking ownership of harness
session data.

Hosted HTTP route control and TUI migration remain Phase 4 and Phase 5. Phase 3
does not introduce a shared ACP service daemon, session database, transcript
store, or claim-attestation mechanism.

## Task 1 — remove the duplicate controller credential

- Delete controller credential issue, authentication, expiry, revoke, and
  `ControllerAuthenticated` event paths.
- Keep ordinary `brvk_*` validation as the only authenticated model-request
  path; preserve the existing early local allow for `skip_auth`.
- Keep the endpoint plan's ordinary API key/launch placeholder instead of
  replacing it after process launch.
- Remove credential-only SDK/application APIs and their tests.

Completion: no `brac_*` value can be issued or accepted, normal API auth tests
and the `skip_auth` truth table pass, and secrets remain redacted.

## Task 2 — API-principal route leases

- Derive an opaque route principal from the ordinary virtual key on both the
  controller and model-request paths; use the singleton `local` principal for
  `skip_auth`.
- Key leases by `(api_principal, declared_controller_id, session_id)`.
- Give every lease an independent TTL and remove it on reset, successful
  close/delete, controller disconnect, expiry, or daemon restart.
- Keep route control on the owner-only local daemon socket in this phase. The
  socket transports route mutations but does not mint a second credential.
- Prove that distinct API principals cannot cross namespaces, equal session
  IDs under distinct controller IDs do not collide, and deliberate reuse of
  exact claims under one principal is accepted.

Completion: runtime, daemon, controller bridge, and integration tests cover
namespace isolation, precedence, cleanup, and expiry.

## Task 3 — declared session identity and observability

- Replace authenticated/trusted controller terminology with declared claim and
  route-match terminology in request context, events, capture, spans, and
  metering plumbing.
- Classify the ACP origin from the declared controller marker and parse
  recognized harness/native evidence independently for attribution; never
  describe either as authentication or attestation.
- Preserve the pure model API compatibility projection and continuation
  precedence unchanged.
- Record every reviewed ACP, Claude, and Codex identity header and whether the
  signal participated in route matching. Exclude authorization, cookies, and
  credentials.

Completion: pure API fixtures remain unchanged; declared ACP and native
signals join request, route, trace, and settlement artifacts without secret
material or high-cardinality metric labels.

## Task 4 — pin a coherent ACP v1 Rust SDK line

The crates.io runtime `agent-client-protocol` 2.0.0 pins schema 1.5.0 exactly,
while schema 1.7.0 is the current Rust schema release. Therefore changing only
the direct schema dependency would create two incompatible type universes and
is not an upgrade.

- Pin `agent-client-protocol` and its conductor to one reviewed official Rust
  SDK revision that consumes schema 1.7.0, and pin the direct schema dependency
  to the same version.
- Keep `ProtocolVersion::V1` and `schema::v1`; do not enable protocol v2.
- Record the exact upstream revision in the dependency and spec.
- Keep `session/fork` behind its explicit unstable feature; provider setup
  remains an internal, version-pinned extension.

Completion: one schema version exists in `Cargo.lock`, the controller compiles
against a coherent official SDK revision, and ACP v2 remains disabled.

## Task 5 — v1 forwarding contracts

Extend the fake-harness protocol suite to prove bidirectional preservation for:

- initialize capabilities and `_meta`, including terminal auth and
  elicitation;
- authenticate and logout;
- filesystem read and write;
- terminal create, output, wait, kill, and release;
- permission callbacks and elicitation callbacks;
- cancellation;
- session modes, configuration options, and additional directories; and
- lifecycle calls and unknown namespaced JSON-RPC extensions.

Typed initialize fields and ACP `_meta` must survive decode/forward/re-encode.
Arbitrary unknown top-level initialize fields are not promised by the typed
upstream SDK and must not be advertised as preserved. Unknown namespaced RPC
methods continue to use the raw dispatch forwarding path.

Completion: contract tests exercise each negotiated surface and verify native
session IDs plus `_meta` round-trip unchanged.

## Task 6 — documentation, review, and release gates

- Correct CLI/help wording so one controller connection is not described as
  one ACP session.
- Update `skills/bitrouter/` for changed auth and harness wiring.
- Run focused tests while implementing, then:
  - `cargo nextest run --all-features`;
  - `cargo clippy --workspace --all-features --tests -- -D warnings`;
  - `cargo fmt -- --check`; and
  - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`.
- Review the final diff for correctness, secret handling, compatibility,
  unnecessary abstraction, and scope creep; fix all Critical and Important
  findings and rerun affected gates.
- Commit conventionally, push the branch, open a Phase 3 PR, and verify its CI.

Completion: the PR contains implementation, tests, spec/skill updates, review
evidence, and successful CI, with hosted control and TUI work left explicit.

## Implementation evidence

- The runtime, conductor, and schema form one ACP v1 type universe at official
  Rust SDK revision `c63610fc38a642f7a73ba2719f403f17d771c345` and schema
  1.7.0; `unstable_protocol_v2` is not enabled.
- No production path issues, accepts, authenticates, renews, or revokes a
  `brac_*` credential. Normal virtual-key authentication and the explicit
  `skip_auth` local principal remain the only request boundaries.
- Focused controller, routing, authentication, daemon, metering, and ACP CLI
  contract suites pass.
- The final local tree passes 2,888 nextest tests with 11 skipped, workspace
  clippy with warnings denied, formatting, rustdoc with warnings denied, and
  `git diff --check`.
- Final Critical/Important review found no remaining correctness, secret
  handling, compatibility, or scope findings. Hosted HTTP route control and
  the TUI remain Phase 4 and Phase 5 respectively.
