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

Past those daemon control commands, v1 uses `bitrouter cloud …` for OAuth sign-in and account operations.

## Sign in to BitRouter Cloud

`bitrouter cloud login` either runs the RFC 8628 Device Authorization Grant or stores an existing BitRouter API key. Both credential types live under `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode `0600` on Unix) and are reused by raw API requests, management commands, the built-in provider, and telemetry attribution. The API-key form performs no network request, which makes it suitable for CI.

```bash
bitrouter cloud login
bitrouter cloud login --api-key "$BITROUTER_API_KEY"
bitrouter cloud whoami
```

Interactive OAuth browser approval lets you pick the workspace this CLI session is bound to. To switch workspaces, run `bitrouter cloud login` again and choose the target workspace. OAuth access tokens refresh automatically within 60 s of expiry. The default authorization server is `https://api.bitrouter.ai`; override it with `--oauth-as <URL>` (or `BITROUTER_OAUTH_AS`) for a self-hosted deployment.

The default OAuth scope set covers `inference:invoke`, `usage:read`, `keys:read`/`keys:write`, `billing:read`, `policy:read`/`policy:write`, `byok:read`/`byok:write`, and `namespace:read`. Sensitive control-plane scopes such as `billing:write`, `user:write`, and `namespace:write` are opt-in via `--scope`. `--api-key` conflicts with OAuth-only `--client-id` and `--scope`.

Inspect the local session with `bitrouter cloud whoami` — it reads the credentials file directly and never hits the network. Sign out with `bitrouter cloud logout`: OAuth credentials are revoked on a best-effort basis before local deletion, while API keys are deleted locally only.

Receipt reconciliation can use the same protected credential file without exporting a bearer into the environment. Pass `--credentials-file "$XDG_DATA_HOME/bitrouter/account-credentials.json"` to `bitrouter workflow-state reconcile-metering`; fresh OAuth tokens are used directly and near-expiry tokens are refreshed before the receipt request. Omitting the flag preserves the existing `--api-key-env BITROUTER_API_KEY` behavior.

<Callout type="info">
After either login form, the `bitrouter` provider is auto-enabled in zero-config mode — every model your account is entitled to is routable as `bitrouter:<model-id>` with no further setup.
</Callout>

## Call Cloud APIs directly

`bitrouter cloud api` is modeled after `gh api`. It accepts an arbitrary relative endpoint on the logged-in origin, injects the stored bearer, and streams the response without requiring a local daemon.

```bash
bitrouter cloud api /v1/models
bitrouter cloud api /v1/chat/completions --input request.json
bitrouter cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

The command supports `-X/--method`, repeated `-H/--header`, `-f/--raw-field`, `-F/--field`, `--input`, `-i/--include`, `--silent`, and `--verbose`. Absolute URLs and fragments are rejected, and redirect following is disabled, to keep the credential on its login origin. Non-TTY JSON and SSE bytes pass through unchanged. See the [Cloud API guide](/docs/guides/cloud-api) for every initial protocol and field/input semantics.

## Manage your account: `bitrouter cloud`

Every leaf accepts `--json` for raw response output; the default is a `systemctl`-style key:value block (single resource) or a small table (lists). When the server returns a 403 with `missing required scope: <s>`, OAuth users receive a copy-pasteable `bitrouter cloud login --scope "<current> <s>"` hint; API-key users are told to mint or select a key with that scope.

OAuth credentials are namespace-baked. API-key credentials use the server's `me` namespace alias for workspace-scoped management routes. `whoami` reports `oauth` or `api_key` without printing the credential; API-key logout only removes the local file.

### `bitrouter cloud whoami`

Identity stored on this machine plus the `/v1/*` base URL the CLI will target. Offline read.

### `bitrouter cloud namespace` — workspaces

```bash
bitrouter cloud namespace list
bitrouter cloud namespace current
```

The credential is namespace-baked: keys, usage, and policies are scoped to the workspace chosen at login. `current` prints `(no namespace — run \`bitrouter cloud login\`)` when the local credential predates namespace binding.

### `bitrouter cloud keys` — API keys

```bash
bitrouter cloud keys list
bitrouter cloud keys mint --name ci --scope "policy:read usage:read"
bitrouter cloud keys revoke <id>
```

`mint` returns the plaintext `brk_…` token exactly once — save it on first read; the server keeps only the SHA-256 hash. Requested scopes must be a subset of your effective scopes (RFC 6749 §3.3 forbids upscaling).

### `bitrouter cloud usage` / `bitrouter cloud requests`

```bash
bitrouter cloud usage
bitrouter cloud usage --from 2026-05-01T00:00:00Z --to 2026-06-01T00:00:00Z
bitrouter cloud requests --limit 50 --offset 0
```

`usage` aggregates spend (micro-USD) and token counts. `requests` pages through the request history.

### `bitrouter cloud billing` — balance + checkout

```bash
bitrouter cloud billing balance
bitrouter cloud billing checkout --amount-cents 2000
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

## Other ways to drive BitRouter

The CLI isn't the only surface onto the daemon. An agent can also drive BitRouter over [MCP](/docs/concepts/mcp) — the origin server exposing `complete`, `list_models`, and `status` as tools — or via the shipped [`/bitrouter` Agent Skill](/docs/concepts/agent-skill), which teaches a coding agent to install and operate BitRouter on its own.
