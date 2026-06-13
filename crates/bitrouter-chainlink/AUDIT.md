# Chainlink Confidential AI — TEE Attestation Security Audit

| | |
|---|---|
| **Target** | Chainlink "Confidential AI Attester Demo" (`https://confidential-ai-dev-preview.cldev.cloud`) and its BitRouter integration (`crates/bitrouter-chainlink`, `plugins/bitrouter-pay`) |
| **Date** | 2026-06-13 |
| **Scope** | Whether BitRouter's Chainlink integration delivers, and truthfully reports, TEE attestation |
| **Methodology** | Live probing of the dev-preview API (with a hackathon key) + source review of the integration |
| **Status of target** | ETHGlobal NY 2026 hackathon dev-preview; explicitly experimental |

> **Verdict:** The Chainlink dev-preview is **not** verifiable TEE attestation. It exposes only *unsigned* SHA‑256 digests — no attestation document, no measurements, no signature, no challenge. Prior to remediation, the BitRouter integration **claimed** TEE attestation it could not substantiate. PR #3 removes those claims and makes the integration honest and fail‑closed. This document records the findings, the trust gap, the remediation, and the roadmap to real attestation.

---

## 1. Executive summary

"Confidential inference" has two distinct security properties:

- **L1 — genuine‑TEE attestation:** proof that the endpoint is real TEE hardware running the *expected* (policy‑pinned) workload. Requires a hardware‑rooted, signed attestation document (for AWS Nitro: an NSM‑signed COSE_Sign1 with PCR measurements, chained to the AWS Nitro PKI).
- **L1.5 — exchange integrity:** proof that a *specific* request/response ran in that TEE unmodified. Requires a signature, by an enclave‑held key bound to the attestation, over the exchange.

The Chainlink dev‑preview provides **neither**. It returns only unsigned per‑resource SHA‑256 digests, and only when documents are attached. Those digests are self‑reported by an untrusted service and carry no enclave identity, so they are **not a trust anchor**.

Despite the product name ("Attester Demo"), the BitRouter integration originally asserted attestation anyway (a `"TEE-attested"` log line; a hardcoded `attested: true` receipt). Those are **false security claims** — the most serious class of finding here, because they would lead a user to trust an exchange that is not actually attested.

---

## 2. Methodology

1. **Live API probing** of the dev‑preview using a provided key:
   - `GET /v1/models`, `POST /v1/inference`, `GET /v1/inference/:id` (full submit→poll round trip).
   - Probed for an attestation surface: `GET /v1/attestation`, `/v1/attestation/report`, `/v1/attest`, `/attestation`, `/v1/inference/:id/attestation` — **all 404**.
   - Inspected completed‑job JSON and HTTP response headers for any signed material.
   - Read the published `/docs`.
2. **Source review** of `crates/bitrouter-chainlink` and `plugins/bitrouter-pay` for how attestation was produced, labelled, and surfaced.

---

## 3. What the dev-preview actually exposes (evidence)

A plain prompt completion (`GET /v1/inference/:id`) returns **no** attestation material:

```json
{ "id": "...", "status": "completed", "model": "gemma4",
  "output": "Hello", "usage": { "prompt_tokens": 54, "completion_tokens": 2 },
  "created_at": "...", "started_at": "...", "completed_at": "..." }
```

Only when **resources (documents)** are attached does the completed job carry digests:

| Field | Meaning | Client‑reproducible? |
|-------|---------|----------------------|
| `digest` | SHA‑256 of the original resource content | **Yes** — the client uploaded the bytes |
| `request_digest` | SHA‑256 of Chainlink's *canonical request metadata* | No — canonicalization unpublished |
| `response_digest` | SHA‑256 of Chainlink's *canonical response metadata* | No — canonicalization unpublished |
| `filename_digest` | SHA‑256 of the blinded filename | No |
| `filename_blinding` | Random hex blinding value | n/a |

**Absent entirely:** Nitro/NSM attestation document, PCR measurements (PCR0/1/2/8), AWS Nitro PKI certificate chain, any signature over the digests, any nonce/challenge mechanism, and any standalone attestation endpoint. No attestation material appears in response headers either.

---

## 4. Findings

| ID | Severity | Title |
|----|----------|-------|
| F‑1 | **High** | Executor asserts `"TEE-attested"` for an unattested exchange |
| F‑2 | **High** | Payment receipt hardcodes `attested: true` over unsigned digests |
| F‑3 | **Medium** | Attestation logic fragmented across three divergent code paths |
| F‑4 | Informational | Dev‑preview exposes no cryptographic attestation (root cause) |
| F‑5 | Informational | Unsigned digests are integrity *evidence*, not a trust anchor |

### F‑1 — Executor asserts "TEE-attested" for an unattested exchange — *High*
**Before:** `ChainlinkExecutor::execute` copied the inference id + lightweight digests into `provider_metadata` and logged `"...completed (TEE-attested)"`. No quote was fetched, no measurement checked, no signature verified — nothing was attested. **Impact:** an operator reading logs (or tooling parsing them) would conclude the exchange ran in a verified TEE when it did not. **Status: fixed** (see §6).

### F‑2 — Payment receipt hardcodes `attested: true` — *High*
**Before:** `bitrouter-pay`'s `ChainlinkAttester` minted an `AttestationReceipt { …, attested: true }` — a constant, not a verification result — and returned it as the paywalled route's response body. **Impact:** a paying caller receives a receipt asserting attestation that was never performed; the strongest false‑trust surface, because money is exchanged for the "attested" property. **Status: fixed** (see §6).

### F‑3 — Fragmented attestation code paths — *Medium*
**Before:** three independent implementations — the executor stash (F‑1), a *duplicate* Chainlink submit/poll client inside `bitrouter-pay` (F‑2), and the new `ConfidentialVerifier` framework that neither used. Divergent behavior, duplicated network code, and no single place to make the trust decision correct. **Status: fixed** — collapsed onto one shared `ChainlinkVerifier` engine.

### F‑4 — No cryptographic attestation in the dev-preview — *Informational (by design, upstream)*
The dev‑preview is explicitly experimental and exposes no signed attestation (§3). This is the root cause that makes F‑1/F‑2 *false* rather than merely sloppy. Not a defect in BitRouter; it bounds what any honest integration can claim today.

### F‑5 — Unsigned digests are evidence, not a trust anchor — *Informational*
The one client‑checkable fact is `sha256(uploaded bytes) == digest`. This detects corruption of an uploaded document *relative to what the service self‑reports*, but because the service is untrusted and nothing is signed, a malicious relay can fabricate consistent digests. It must never be presented as proof of genuine‑TEE execution.

---

## 5. Trust-gap analysis

| Question | Answer today |
|----------|--------------|
| Is the endpoint proven to be genuine TEE hardware? | **No** — no attestation document. |
| Is it proven to run the expected workload? | **No** — no PCR measurements / policy pin. |
| Is the response proven to come from the enclave unmodified? | **No** — no enclave signature over the exchange. |
| Can the client detect tampering of an uploaded document vs the service's report? | Partially — `digest` is reproducible, but the comparison baseline is itself unsigned/untrusted. |
| Is there replay/freshness protection? | **No** — no nonce/challenge. |

**Conclusion:** confidentiality may hold inside the enclave, but from the client's perspective there is currently **no verifiable attestation**. The correct posture is fail‑closed: report `verified = false` and never imply otherwise.

---

## 6. Remediation (delivered in PR #3)

| Finding | Fix |
|---------|-----|
| F‑1 | Removed the `"TEE-attested"` log and `stash_attestation`. The executor now records digests as **neutral evidence** in `provider_metadata` (data, not a verdict). |
| F‑2 | Deleted `AttestationReceipt { attested: true }` and the duplicate client. The pay gate now returns an honest `VerifiedExchange` whose `verified` is **always `false`** while digests are unsigned. |
| F‑3 | Single shared engine: `ChainlinkVerifier: ConfidentialVerifier`. `verify_attestation` (L1) returns fail‑closed `unverified`; `verify_exchange` (L1.5) re‑checks only the reproducible `digest` and reports `digests_consistent`, with `verified = false`. Both the CLI and the pay gate use it. |
| F‑5 | Evidence is labelled honestly throughout (README, CLI output: "TEE‑attested (always ✗ — dev‑preview exposes no signed quote)"). |

**Verification posture:** L1/Enforce remains the only ambient, pre‑execution gate (drops a target rather than emitting a verdict). Exchange verification is opt‑in via `bitrouter verify-exchange`. No false claim survives anywhere (`TEE-attested` / `attested: true` / `stash_attestation` greps are empty).

---

## 7. Roadmap to real attestation

The integration is structured so that when Chainlink exposes real attestation, **only `ChainlinkVerifier` + the wire types change** — no caller, registry, plugin, or executor changes — and `Record → Enforce` becomes meaningful (`verified` can finally be `true`).

**Server side (Chainlink must provide):**
1. A Nitro/NSM **COSE_Sign1 attestation document** with PCR measurements, chained to the AWS Nitro PKI, bound to a **client nonce** and carrying an **enclave public key** — ideally via a standalone `GET /v1/attestation?nonce=` so L1 can run *before* inference.
2. **Per‑inference signatures** by the enclave key (whose public half is in the attestation doc) over the exchange digests.
3. **Published canonicalization** of request/response metadata, so `request_digest`/`response_digest` become client‑reproducible.
4. Documented **enclave key provisioning** (attestation‑gated key release) so the signing‑key ↔ measurement binding is itself trustworthy.

**Client side (`ChainlinkVerifier` fills in):**
1. Verify the COSE_Sign1 against the AWS Nitro root; check the echoed nonce (freshness).
2. Pin PCRs against an allowlist policy (analogous to the NEAR DCAP policy), constructed fail‑closed so it refuses to run unpinned.
3. Bind the enclave public key as the attested signer; verify the per‑inference signature recovers to it.
4. Only then set `verified = true`.

---

## 8. Residual risk & recommendations

- **Until the server roadmap lands, treat Chainlink confidential inference as confidentiality‑only**, with *no* verifiable attestation. Keep the default policy at **Record**; only move to **Enforce** once `verified` can be `true`, at which point Enforce correctly drops unattested targets.
- Do not surface `digests_consistent` to end users as "attested" or "verified" — it is tamper‑evidence against an untrusted baseline, nothing more.
- Re‑run this audit when Chainlink ships a non‑demo API; §3's probing is the regression test for "did attestation material appear yet?"
