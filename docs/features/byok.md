---
title: External providers (BYOK)
description: Bring your own provider key — use your own account, pay the provider directly, and let BitRouter route requests without a rev share or per-token fee. Works self-hosted, locally, and on BitRouter Cloud.
sourceHash: db475e399f3b5f33095b1ac876df326a5cfe0654d33aff75b1583070724779f1
---

**BYOK** (bring your own key) routes your requests using your own provider account, not BitRouter's. You pay the provider directly at their list price — BitRouter takes no rev share, adds no per-token fee, and never holds your keys in plaintext.

BYOK works the same way regardless of deployment:

- **Self-hosted / local binary** — set provider keys in the environment. BitRouter detects them at startup; no upload or encryption step.
- **BitRouter Cloud** — keys are encrypted client-side with a sealed box before submission. The node only ever sees ciphertext.

## Local mode — env-var auto-detection

When you self-host or run the binary locally there is no upload step: set provider keys in the environment and you are done. No config file required.

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GOOGLE_API_KEY=AIza...
bitrouter
```

BitRouter detects keys at startup and exposes only the providers whose keys are present. If a provider's key is missing, attempts to route to that provider return `402 Payment Required` with a structured error pointing at the missing variable.

### Recognized variables

| Provider | Preferred name | Passthrough fallback |
| --- | --- | --- |
| OpenAI | `BITROUTER_OPENAI_API_KEY` | `OPENAI_API_KEY` |
| Anthropic | `BITROUTER_ANTHROPIC_API_KEY` | `ANTHROPIC_API_KEY` |
| Google | `BITROUTER_GOOGLE_API_KEY` | `GOOGLE_API_KEY`, `GEMINI_API_KEY` |
| Custom (registry-listed) | `BITROUTER_<PROVIDER_ID>_API_KEY` | — |

The `BITROUTER_*` variants take precedence over the passthrough names — useful when your shell already has `OPENAI_API_KEY` set for a different tool and you want BitRouter to use a different account.

For a custom provider registered as `id: my-provider` in the [registry](/docs/guides/register-as-a-provider), set `BITROUTER_MY_PROVIDER_API_KEY` (uppercased, hyphens become underscores).

<Callout type="info">
**Key rotation is live.** BitRouter watches the environment of its parent process; updating a key (re-export and `kill -HUP $(pgrep bitrouter)`) takes effect on the next request without restart.
</Callout>

## On BitRouter Cloud

On `cloud.bitrouter.ai`, your provider key is encrypted **client-side** against the node's X25519 sealed-box public key before submission. The node never sees plaintext on the write path; ciphertext is decrypted in-memory at request time and never logged.

The flow:

1. **Fetch the node's public key.** [`GET /v1/byok/encryption-pubkey`](/docs/reference/byok/getEncryptionPubkey) returns the current X25519 public key and a `kek_id` fingerprint. Cache by fingerprint and pass it back as `If-None-Match` to short-circuit on `304 Not Modified`.
2. **Encrypt the plaintext key.** Use libsodium `crypto_box_seal` (or any sealed-box implementation) against the public key.
3. **Submit the ciphertext.** The [console](https://cloud.bitrouter.ai) does steps 1–2 in-browser when you paste a key — you never need to leave the dashboard. The same submission API is also available for scripted onboarding; see the [API reference](/docs/reference/byok/getEncryptionPubkey).

### Encryption recipe

If you're scripting key submission, here's the minimum sealed-box step. The output ciphertext is what the submission endpoint expects.

<Tabs items={['Node.js', 'Python', 'Shell']}>
<Tab value="Node.js">

```js
import sodium from 'libsodium-wrappers';

await sodium.ready;

const meta = await fetch(
  'https://cloud.bitrouter.ai/v1/byok/encryption-pubkey'
).then(r => r.json());

const ciphertext = sodium.crypto_box_seal(
  sodium.from_string(process.env.OPENAI_API_KEY),
  sodium.from_base64(meta.public_key, sodium.base64_variants.ORIGINAL)
);

const ciphertextB64 = sodium.to_base64(ciphertext, sodium.base64_variants.ORIGINAL);
// Submit { provider_name: 'openai', kek_id: meta.kek_id, ciphertext_b64: ciphertextB64, key_prefix: 'sk-...' }
```

</Tab>
<Tab value="Python">

```python
import os, base64, requests
from nacl.public import PublicKey, SealedBox

meta = requests.get('https://cloud.bitrouter.ai/v1/byok/encryption-pubkey').json()
pubkey = PublicKey(base64.b64decode(meta['public_key']))
ciphertext = SealedBox(pubkey).encrypt(os.environ['OPENAI_API_KEY'].encode())

payload = {
    'provider_name': 'openai',
    'kek_id': meta['kek_id'],
    'ciphertext_b64': base64.b64encode(ciphertext).decode(),
    'key_prefix': 'sk-...',
}
# POST `payload` to the BYOK submission endpoint.
```

</Tab>
<Tab value="Shell">

```bash
META=$(curl -s https://cloud.bitrouter.ai/v1/byok/encryption-pubkey)
PUBKEY=$(echo "$META" | jq -r .public_key)
KEK_ID=$(echo "$META" | jq -r .kek_id)

CIPHERTEXT=$(printf '%s' "$OPENAI_API_KEY" \
  | sodium-seal --pubkey "$PUBKEY" \
  | base64)

# Submit { provider_name: "openai", kek_id: "$KEK_ID", ciphertext_b64: "$CIPHERTEXT", key_prefix: "sk-..." }
```

</Tab>
</Tabs>

The pubkey endpoint honors `If-None-Match` for cheap caching — pin the `kek_id` from a prior response and you'll get `304 Not Modified` while the key is unchanged.

### Key scope

Keys are scoped to your **user account** — every API key and OAuth token issued under your account can route requests through the keys you have stored. No one can read the ciphertext or the raw key.

### Rotation, revocation, and audit

- **Rotate** by submitting a new ciphertext for the same provider — the previous record is overwritten atomically.
- **Revoke** from the dashboard. In-flight requests using the prior key complete; new requests get `402 Payment Required`.
- **Audit** — every submission is recorded with its time and the `kek_id` used. Plaintext is never visible to anyone, including BitRouter operators.

If a node's `kek_id` rotates (we re-key every 90 days), the previous key is retained in memory so already-submitted ciphertexts remain decryptable at request time. New submissions must use the current `kek_id`; re-encrypt only if you explicitly want to migrate to the new key.

## Custom providers

BYOK works for any provider listed in the [registry](/docs/guides/register-as-a-provider) that declares `byok` in its manifest's `payment.modes`. The provider field in the encryption submission must match the registry `id` exactly. Local-mode env-var detection follows the `BITROUTER_<PROVIDER_ID>_API_KEY` convention.
