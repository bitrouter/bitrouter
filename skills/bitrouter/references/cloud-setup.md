# Cloud setup

The user chose **BitRouter Cloud** (managed proxy at `api.bitrouter.ai`, one bill, no per-provider keys). This file covers the four entry points and the operational details for each: web playground (A), dashboard-minted API key in code (B), permissionless wallet (C), and headless CLI sign-in (D — recommended for terminal-first users).

## A. Web playground (no install, no code)

Fastest demo path. Send the user here:

1. Visit <https://bitrouter.ai>.
2. Sign up (email / OAuth — handled by the console's better-auth).
3. Top up credits via Stripe checkout on the console's billing page.
4. Use the in-browser playground to send chat requests against any supported model.

No SDK, no local daemon, no API key. Stop here if the user just wants to evaluate or run one-off prompts.

## B. API key (`brk_*`) for SDK use — the production path

This is the path agents should default to for any "I want to call BitRouter from my code" question.

### Mint a key

1. <https://bitrouter.ai> → Dashboard → API Keys.
2. Click "Create key", give it a device name (e.g. `laptop`, `prod-worker`).
3. Copy the `brk_…` string immediately — only its SHA-256 hash is stored, so the plaintext is shown once.

### Use it in code

```python
from openai import OpenAI

client = OpenAI(
    base_url="https://api.bitrouter.ai/v1",
    api_key="brk_...",
)
client.chat.completions.create(
    model="openai/gpt-4o",
    messages=[{"role": "user", "content": "hi"}],
)
```

Anthropic SDK:

```python
from anthropic import Anthropic

client = Anthropic(
    base_url="https://api.bitrouter.ai",   # no /v1
    api_key="brk_...",
)
client.messages.create(
    model="anthropic/claude-sonnet-4-5",
    max_tokens=256,
    messages=[{"role": "user", "content": "hi"}],
)
```

### Operate the key

- **Rotation / revocation:** Dashboard → API Keys → revoke (per-row). Keys are matched by their `sha256` hash; a revoked key's hash can be reused by a new key without collision.
- **Billing:** every request decrements the user's Postgres `credit_balances` row by the cost of the actual tokens consumed (not an estimate). Top-up via Stripe in the console — credits are tracked in cents.
- **Overdraft:** the gate allows a small overdraft if many concurrent requests collectively exceed the balance estimate, but the next request after the balance hits zero will be rejected. Top up before running large batches.

## C. Permissionless wallet — no signup, crypto-native

For users who want to skip account creation entirely. Sign a JWT with a Solana (Ed25519) or EVM (secp256k1) wallet; BitRouter's `bitrouter-node` verifies the signature against the CAIP-10 `iss` claim and routes the request. Payment goes through x402 / MPP escrow on-chain.

This is fiddly to script — recommend pointing the user at <https://bitrouter.ai> docs rather than writing the JWT signing flow inline. Key facts the agent should know without going deeper:

- The `iss` claim is CAIP-10: `solana:<base58 pubkey>` or `eip155:1:<0x address>`.
- The JOSE `alg` is `SOL_EDDSA` (BitRouter-native, distinct from RFC 7515's `EdDSA`).
- Cross-`alg` forgery (wallet `iss` with `EdDSA`, or thumbprint `iss` with `SOL_EDDSA`) is rejected before signature verification, so don't try to "fix" mismatches by changing the header.
- Balance lives in a Mongo `charge_balances` collection keyed by wallet — separate from the Stripe-credit Postgres path.

## D. Headless CLI (`bitrouter cloud login`) — recommended for terminal-first users

This is the seam between Local and Cloud, closed by `bitrouter cloud login` (see also `references/cli.md`).

### Sign in

```bash
bitrouter cloud login
# Open this URL in your browser:
#   https://cloud.bitrouter.ai/oauth/device?user_code=ABCD-EFGH
# Waiting for authorization (the code expires in 600s)…
```

Mechanism: RFC 8628 device-authorization grant against the AS advertised at `https://api.bitrouter.ai/.well-known/oauth-authorization-server`. The CLI polls the token endpoint, exchanges the device code for an access + refresh token pair, and persists both to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Every subsequent call auto-refreshes within 60 s of access-token expiry — sign in once per machine.

Override the AS for a self-hosted deployment: `bitrouter cloud login --oauth-as https://my-self-hosted.example.com`.

Sensitive scopes are off the default grant set — opt in at login time:

```bash
# To use `bitrouter cloud billing checkout`:
bitrouter cloud login --scope "inference:invoke usage:read keys:read keys:write \
                              billing:read billing:write \
                              policy:read policy:write byok:read byok:write \
                              namespace:read"
```

### Auto-enable for the local daemon

When the credentials file is present, the local `bitrouter` daemon auto-adds the `bitrouter` provider to the in-memory zero-config providers map (see `apps/bitrouter/src/cloud/mod.rs::enable_in_zero_config`). Every model the account is entitled to becomes routable as `bitrouter:<model-id>` against `http://localhost:4356` — no `bitrouter.yaml` changes, no `BITROUTER_API_KEY` env var.

```python
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
client.chat.completions.create(
    model="bitrouter/gpt-4o",      # served via the user's cloud subscription
    messages=[{"role": "user", "content": "hi"}],
)
```

Cloud and BYOK coexist — providers with `${PROVIDER_API_KEY}` in the environment stay configured alongside the cloud provider.

### Manage the account from the terminal

After sign-in, `bitrouter cloud …` covers the full `/v1/*` management API:

```bash
bitrouter cloud whoami                 # cloud base URL + local subject/scope
bitrouter cloud usage                  # last 30 days of spend (micro-USD)
bitrouter cloud keys list              # provisioned brk_… API keys
bitrouter cloud keys mint --name laptop --scope "policy:read"
bitrouter cloud billing balance        # credit balance
bitrouter cloud policy list            # account-bound policies
bitrouter cloud byok list              # BYOK provider keys
```

Every leaf accepts `--json` for raw response output. See `references/cli.md` for the full subcommand index.

### Sign out

```bash
bitrouter cloud logout
```

Runs an RFC 7009 best-effort revoke at the AS, then deletes the local credentials file. Idempotent — re-running with no credentials present is a no-op.

## What Cloud does for the user that Local doesn't

- **No provider keys to manage.** One `brk_*` instead of `OPENAI_API_KEY` + `ANTHROPIC_API_KEY` + `GEMINI_API_KEY` + …
- **One bill.** Stripe credits for fiat, x402 wallet for crypto. No reconciling N upstream invoices.
- **Curated registry.** Cloud uses a pinned `provider-registry` baked into the deploy image, so model availability and protocol routing match what the playground previews.
- **No daemon to keep alive.** No `bitrouter status`, no `~/.bitrouter/`, no log rotation.

## What Local does for the user that Cloud doesn't

- **No third party in the request path.** Latency, privacy, audit trail all stay on the user's machine.
- **Custom providers.** Local can target Ollama, Azure OpenAI deployments, internal endpoints — Cloud's registry is fixed.
- **MCP / ACP wiring.** `mcp_servers:` and `agents:` in `bitrouter.yaml` are a Local-only feature today.
- **No usage cost above what the upstream charges.** BYOK is at-cost; Cloud adds the platform's margin.

## Switching between Local and Cloud

The SDK base URL is the only thing that changes — model identifiers and SDK code are identical:

```python
# Local
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")

# Cloud
client = OpenAI(base_url="https://api.bitrouter.ai/v1", api_key="brk_...")

# both:
client.chat.completions.create(model="openai/gpt-4o", messages=[...])
```

A user can run both side-by-side (different base URLs in different environments) — Cloud for production, Local for offline development, no code branching needed.
