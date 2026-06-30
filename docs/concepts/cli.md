---
title: CLI
description: The single local binary that runs BitRouter — one endpoint your runtime points at, a daemon you control, and a scriptable surface for cloud sign-in and account management.
sourceHash: 35a2dabbe22e0eb9fec67078169dab4fb7c0b5cc47d34d9b24aa959c5453f90a
---

BitRouter ships as one **static binary**, `bitrouter`, with no dependencies to install. It plays two roles: it runs the **local router** your agent talks to, and it's the **command-line surface** for your hosted account.

## The local endpoint

Your agent never talks to a remote API directly — it points at the binary running locally, by default on `http://127.0.0.1:4356`. Everything else in these docs — the four model protocols, the MCP and ACP gateways — is served from that one endpoint.

You run it as a daemon and control its lifecycle:

- `bitrouter serve` — run the router in the foreground.
- `bitrouter start` / `stop` / `status` — manage it as a background daemon.

Past those daemon control commands, v1 ships two cloud-facing surfaces: `bitrouter auth …` for OAuth sign-in, and `bitrouter cloud …` for everything you can do once signed in.

## Sign in to BitRouter Cloud

`bitrouter auth login` runs the RFC 8628 Device Authorization Grant against the configured authorization server, prints an approval URL, and persists the resulting access + refresh tokens under `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode `0600` on Unix). The token is refreshed automatically within 60 s of expiry — you sign in once per machine.

```bash
bitrouter auth login
# Open this URL in your browser:
#   https://cloud.bitrouter.ai/oauth/device?user_code=ABCD-EFGH
# Waiting for authorization (the code expires in 600s)…
```

The default authorization server is `https://api.bitrouter.ai`. Override with `--oauth-as <URL>` (or `BITROUTER_OAUTH_AS`) for a self-hosted deployment. The default scope set covers inference plus the read/write sides of keys, usage, billing-read, policy, BYOK, and account-read — opt in to the sensitive scopes (`billing:write`, `account:write`, `clients:read`, `clients:write`) by passing `--scope "<existing> clients:write"` at login time.

Inspect the local session with `bitrouter auth whoami` — it reads the credentials file directly and never hits the network. Sign out (best-effort revoke at the AS plus delete the local file) with `bitrouter auth logout`.

<Callout type="info">
After `bitrouter auth login`, the `bitrouter` provider is auto-enabled in zero-config mode — every model your account is entitled to is routable as `bitrouter:<model-id>` with no further setup.
</Callout>

## Manage your account: `bitrouter cloud`

Every leaf accepts `--json` for raw response output; the default is a `systemctl`-style key:value block (single resource) or a small table (lists). When the server returns a 403 with `missing required scope: <s>`, the CLI prints a copy-pasteable `bitrouter auth login --scope "<current> <s>"` hint.

### `bitrouter cloud whoami`

Identity stored on this machine plus the `/v1/*` base URL the CLI will target. Offline read.

### `bitrouter cloud keys` — API keys

```bash
bitrouter cloud keys list
bitrouter cloud keys mint --name ci --scope "policy:read usage:read"
bitrouter cloud keys revoke <id>
```

`mint` returns the plaintext `brk_…` token exactly once — save it on first read; the server keeps only the SHA-256 hash. Requested scopes must be a subset of your effective scopes (RFC 6749 §3.3 forbids upscaling).

### `bitrouter cloud usage` / `bitrouter cloud requests`

```bash
bitrouter cloud usage                                       # last 30 days
bitrouter cloud usage --from 2026-05-01T00:00:00Z --to 2026-06-01T00:00:00Z
bitrouter cloud requests --limit 50 --offset 0
```

`usage` aggregates spend (micro-USD) and token counts. `requests` pages through the request history.

### `bitrouter cloud billing` — balance + checkout

```bash
bitrouter cloud billing balance
bitrouter cloud billing checkout --amount-cents 2000       # needs billing:write
```

`checkout` returns a hosted Stripe URL. Requires the `billing:write` scope (not in the default set — re-login with `--scope`).

### `bitrouter cloud policy` — generic policy CRUD

```bash
bitrouter cloud policy list [--kind budget|rate-limit|guardrail|preset]
bitrouter cloud policy get <id>
bitrouter cloud policy create --name nightly-cap --kind budget --spec spec.json
bitrouter cloud policy update <id> [--name X] [--spec spec.json]
bitrouter cloud policy delete <id>
bitrouter cloud policy bind <id> --principal-type api_key --principal-id <key-id>
bitrouter cloud policy unbind <id> <binding-id>
bitrouter cloud policy disable <id>
bitrouter cloud policy enable <id>
bitrouter cloud policy bindings <id>
bitrouter cloud policy effective --principal-type api_key --principal-id <key-id>
bitrouter cloud policy for-principal api_key <key-id>
```

`--spec` reads a JSON file (or `-` for stdin) holding the flat inner spec body — e.g. `{"window": "day", "limit_micro_usd": 5000000}` for a budget. The `effective` and `for-principal` endpoints answer "what would happen to a request from this principal" without making an actual inference call.

### `bitrouter cloud budget` / `bitrouter cloud preset` — typed sugar

Flat wire shapes over budget-kind and preset-kind policies:

```bash
bitrouter cloud budget create --name nightly-cap --window day --limit-micro-usd 5000000
bitrouter cloud preset create --name engineering --guardrail guardrail.json --budget budget.json
```

These hit the same database rows as `bitrouter cloud policy create --kind budget|preset` — pick whichever shape is more convenient for the call site.

### `bitrouter cloud byok` — BYOK provider keys

```bash
bitrouter cloud byok list
bitrouter cloud byok set --provider anthropic \
  --ciphertext-b64 <base64> --kek-id <current-kek> --key-prefix sk-ant-
bitrouter cloud byok delete <provider>
```

Ciphertext must be sealed against the cloud's current X25519 public key before submission — the server only stores already-encrypted bytes. Fetch the current public key from `GET /v1/byok/encryption-pubkey` before sealing.

### `bitrouter cloud oauth-client` — OAuth client registrations

```bash
bitrouter cloud oauth-client list
bitrouter cloud oauth-client register \
  --name "my-agent" --type confidential \
  --grant authorization_code --grant refresh_token \
  --scope inference:invoke --scope policy:read \
  --redirect-uri https://my-agent.example.com/cb
bitrouter cloud oauth-client update <client-id> --name "renamed"
bitrouter cloud oauth-client delete <client-id>
```

Requires the `clients:read` / `clients:write` scopes — not in the default set. The freshly minted `client_secret` for confidential clients is returned exactly once in the `register` response.
