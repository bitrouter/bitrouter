# Resource tenancy implementation

This tracks implementation of the contracts in [M0](RESOURCE_TENANCY_M0.md).
The baseline is `38ddfd231f053beae9b52a61443a3706688628e0`; the implementation
continues the existing service/crate, HTTP/2 and shared-concurrency refactors.
All stages stay in the same stacked change. A checked stage requires runtime
evidence and independent review; fixtures alone do not satisfy runtime gates.

## Delivery stages

- [x] M0: frozen contracts, ownership inventory, migration example, adversarial
  scenarios and local benchmark profile.
- [x] M1a: public value types and bounded HTTP SDK with contract conformance.
- [ ] M1b: persistent local authority, protected HTTP commands, roles/scopes,
  managed writers, CAS/idempotency, secret-once issuance and revocation fencing.
- [ ] M2a: owned drafts, immutable revisions, CAS activation/rollback, pinned
  execution envelopes, configuration validation/apply/read through SDK and CLI.
- [ ] M2b: ownership for all inventory families, including evaluation, evolution,
  scheduling, trajectories, continuations, credentials, caches and background work.
  Harness sessions and tool execution remain client-owned.
- [ ] M3: bounded strict execution facts, actual per-attempt targets/usage,
  durable completion, owned lookup, audit access and conservative crash recovery.
- [ ] Migration: additive schemas, explicit legacy mappings, quarantine of
  ambiguous owners, equal-name/hash isolation, writer fencing and recovery guide.
- [ ] Qualification: feature/dependency boundaries, local compatibility,
  cross-tenant and lifecycle races, HTTP/1.1/h2c JSON/SSE, slow clients and faults.
- [ ] Performance: five paired/interleaved repetitions of the frozen benchmark
  profile, external raw reports and recorded acceptance results.
- [ ] Final independent review and clean CI on the final candidate.

## Acceptance evidence

M1a adds distinct bounded identities, strict nullable CAS inputs, typed grants,
configuration and execution facts in the dependency-light contract crate. The
optional schema feature preserves the frozen wire constraints; its packaged
definitions are checked against the independent M0 schema. Exact integral JSON
spellings are accepted without floating-point rounding of revisions or usage.
The SDK implements all 16 control operations with bounded responses, explicit
installation binding, no redirects/retries, fresh capability discovery and
unknown-outcome recovery. It verifies returned owners, mutation targets,
activation revisions and pagination bounds before returning success.

Validation: 129 frozen fixtures (including 36 semantic rejections), eight direct
contract tests, 11 socket-level SDK tests including HTTP/1.1 and h2c, and the
existing contract/SDK/helper suites passed. The minimal feature profile passed
the contract and HTTP/1.1 tests. Strict all-target Clippy and an independent
adversarial review covered the public-contract/SDK stage; findings about schema
fidelity, integer representation, mutation-result identity and issued credentials
were corrected and re-reviewed. This establishes client/value conformance only;
live authority, dispatch and receipt guarantees remain pending below.

The 16 operations and 35 runtime scenarios in `contracts/resource-tenancy/` are
the minimum conformance inventory. Every family in `inventory.json` needs a
runtime ownership decision and a regression test before M2 is complete.

Ordinary local execution keeps its existing zero-configuration behavior. New
hosted operations fail closed on missing authority, stale revisions or storage
failure. No anonymous fallback, implicit tenant merge, cross-tenant content-hash
lookup or automatic retry of uncertain inference is permitted. Capabilities are
advertised only after their corresponding dispatch guarantees exist.

Ordinary local performance budgets are at most 3% throughput regression,
TTFT p99 growth of `max(5%, 1 ms)`, and RSS growth of `max(5%, 4 MiB)`.
Strict execution evidence must retain at least 85% of ordinary authenticated
throughput, with TTFT p99 growth of `max(10%, 2 ms)` and receipt commit p99 at
most 20 ms on the frozen local reference. Measurements use 15 s warmup, 60 s
sampling and 30 s drain at concurrency 1/16/64/128. These are pending gates,
not claims established by M0 smoke tests.

## Boundaries

Public DTOs stay in `bitrouter-ai`; HTTP transport stays in `bitrouter-sdk`;
authority, persistence, compilation, dispatch and receipts stay in
`apps/bitrouter`. Base gateway includes local tenancy and strict receipts;
`core` adds scheduling/learning; `tui -> harness -> mcp` and optional,
default-on `http2` remain unchanged. No new runtime crate or external database
is required. Identity-provider login, commercial policy and accounting remain
external services. Single-instance operation is the acceptance target; distributed
coordination is reserved without implementing it in this change.
