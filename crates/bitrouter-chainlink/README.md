# bitrouter-chainlink (demo)

OpenAI/Anthropic/Gemini-compatible access to Chainlink Confidential Inference
(TEE-backed, async submit-then-poll) via BitRouter. Hackathon demo; off by
default, isolated from cloud/registry.

## Run

```bash
export CHAINLINK_CONFIDENTIAL_API_KEY=<key>

# Foreground (logs to stdout):
cargo run -p bitrouter --features chainlink-demo -- \
    serve -c crates/bitrouter-chainlink/examples/chainlink.yaml

# Background daemon:
cargo run -p bitrouter --features chainlink-demo -- \
    start -c crates/bitrouter-chainlink/examples/chainlink.yaml
```

## Smoke test (chat-completions)

```bash
curl localhost:4356/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gemma4",
    "messages": [{"role":"user","content":"Say hi in five words."}]
  }'
```

## Smoke test (Anthropic messages)

```bash
curl localhost:4356/v1/messages \
  -H 'content-type: application/json' \
  -d '{
    "model": "gemma4",
    "max_tokens": 64,
    "messages": [{"role":"user","content":"Say hi in five words."}]
  }'
```

## Config

The example config lives at `crates/bitrouter-chainlink/examples/chainlink.yaml`.
Key fields:

| Field | Value |
|---|---|
| `api_base` | `https://confidential-ai-dev-preview.cldev.cloud` |
| `api_protocol` | `chainlink_confidential` |
| `api_key` | `${CHAINLINK_CONFIDENTIAL_API_KEY}` |
| Models | `gemma4`, `qwen3.6` |

## Notes

- The `chainlink-demo` feature is required at build time and is excluded from the
  default feature set, the provider registry, and cloud deployments.
- The Chainlink executor uses an async submit-then-poll pattern: `POST /v1/inference`
  returns a job id, which is then polled at `GET /v1/inference/{id}` until
  `status == "completed"` or an error is returned.
- No live key is required to build; the executor is compiled in only when the
  feature flag is set.

## Attestation status & roadmap

> Full security audit (findings, evidence, trust-gap analysis, remediation): [`AUDIT.md`](./AUDIT.md).

### Current status: unsigned digests only

The Chainlink dev-preview (ETHGlobal NY 2026) is billed as an "Attester Demo"
running on AWS Nitro Enclaves. In practice, verified live against
`https://confidential-ai-dev-preview.cldev.cloud`:

- A plain completion returns **no** attestation document, **no** PCRs, **no**
  signature, **no** nonce/challenge. Probed `/v1/attestation*` routes return
  all 404. There is no standalone attestation endpoint.
- The only integrity artifacts are **unsigned SHA-256 digests**, present **only
  when documents are attached** as resources: `digest` (sha256 of the original
  file content), `request_digest`/`response_digest` (sha256 of canonical
  request/response metadata), `filename_digest`, `filename_blinding`.

**Consequence: there is nothing cryptographically signed to verify.**

`ChainlinkVerifier` is honest and fail-closed about this:

- `verify_attestation` (L1) always returns `unverified` — no quote to fetch.
- `verify_exchange` (L1.5) re-checks the one client-reproducible fact:
  `sha256(uploaded resource bytes) == reported resource digest` → reported as
  `digests_consistent`. The `verified` field is **always `false`** because the
  digests are unsigned. The verifier never claims "TEE-attested".
- In attestation-plugin **Record** mode, Chainlink requests proceed tagged
  "unattested". In **Enforce** mode, Chainlink targets are dropped — the correct
  fail-closed behavior when the caller demands attestation a provider cannot supply.

The `bitrouter verify-exchange` CLI reflects this honestly: it prints
`TEE-attested: ✗` and shows the `digests_consistent` result as the only real
check. See `skills/bitrouter/references/cli.md` for full CLI docs.

### What real Nitro attestation would require (server side)

1. **A Nitro attestation document** — ideally via
   `GET /v1/attestation?nonce=<hex>`, returning a COSE_Sign1 document (from the
   Nitro Security Module) containing PCR measurements signed by the AWS Nitro
   Attestation PKI, plus the echoed nonce and an enclave **public key** in
   `public_key`/`user_data`. This enables real L1 attestation at route time.
2. **Per-inference signatures** — each completed inference response signed by the
   enclave key whose public half is in the attestation document, directly analogous
   to NEAR's per-chat signature over `{model}:{sha256(req)}:{sha256(resp)}`.
3. **Published canonicalization** of the request/response metadata so
   `request_digest`/`response_digest` become client-reproducible (currently
   unpublished, so they cannot be independently recomputed).
4. **Documented enclave key provisioning** — KMS attestation-gated key release so
   the signing-key ↔ PCR binding is itself trustworthy.

### What `ChainlinkVerifier` would then do (client side)

Once the server exposes the above, filling in `ChainlinkVerifier` is the only
change needed — no callers, registry, plugin, or executor changes are required:

- `verify_attestation` (L1) becomes real:
  1. Verify the COSE_Sign1 signature against the **AWS Nitro root certificate**.
  2. Check that the nonce is echoed correctly (freshness binding).
  3. Check PCRs against a **pinned policy** — a `ChainlinkEnclavePolicy` modeled
     on `AciDcapVerifierPolicy` (`accepted_pcr0/1/2`, optional `pcr8`),
     constructed fail-closed so it refuses to run unpinned.
  4. Bind the enclave public key from the document as the attested signer.
  5. Set `verified = true` and populate `attested_addresses` with the enclave key.
- `verify_exchange` (L1.5) becomes real: verify the per-inference signature
  recovers to the attested key from L1 and matches the digests over the exact
  uploaded bytes → set `VerifiedExchange.verified = true`.
- TTL-cache L1 verdicts (analogous to `NearVerifier::verdict_cached`).

**Key property:** once `verified` can be true, Record→Enforce mode becomes
meaningful with no further changes to the attestation plugin, route hook, or
executor. The architecture already has the right seam; only the
`ChainlinkVerifier` impl and its wire types need to fill in.
