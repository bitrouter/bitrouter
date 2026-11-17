# Cloud setup

Browser-approved Agent connections are an opt-in auth-only path; see
[section D](#d-browser-approved-agent-auth). Keep them separate from developer
login and API keys. They do not activate inference routing or spending.

The user chose **BitRouter Cloud** (managed proxy at `api.bitrouter.ai`, one bill, no per-provider keys). This file covers the three entry points and the operational details for each: web playground (A), dashboard-minted API key in code (B), and headless CLI sign-in (C — recommended for terminal-first users).

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
3. Copy the `brk_…` string immediately — it is shown once and only a server-side verifier is retained.

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

- **Rotation / revocation:** Dashboard → API Keys → revoke the relevant key, then update every client that used it.
- **Billing:** usage is settled against the account's prepaid credit balance. Top up through Stripe checkout in the console.
- **Capacity:** top up before running large batches so available credit does not block a request.

## C. Headless CLI (`bro cloud login`) — recommended for terminal-first users

This is the seam between Local and Cloud, closed by `bro cloud login` (see also `references/cli.md`).

### Sign in

```bash
bro cloud login
# Open this URL in your browser:
#   https://cloud.bitrouter.ai/oauth/device?user_code=ABCD-EFGH
# Waiting for authorization (the code expires in 600s)…
```

For CI or another non-interactive environment, store an existing BitRouter key without discovery or browser approval:

```bash
bro cloud login --api-key "$BITROUTER_API_KEY"
bro cloud whoami
```

`--api-key` conflicts with `--client-id` and `--scope`. It accepts `--oauth-as` for a self-hosted origin. Both OAuth and API-key credentials use the same mode-0600 file; API keys are never printed. API-key logout removes only the local file.

Mechanism: RFC 8628 device-authorization grant against the AS advertised at `https://api.bitrouter.ai/.well-known/oauth-authorization-server`. The CLI polls the token endpoint, exchanges the device code for an access + refresh token pair, and persists both to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Every subsequent call auto-refreshes within 60 s of access-token expiry — sign in once per machine.

Override the AS for a self-hosted deployment: `bro cloud login --oauth-as https://my-self-hosted.example.com`.

Sensitive scopes are off the default grant set — opt in at login time:

```bash
# To use `bro cloud billing checkout`:
bro cloud login --scope "inference:invoke usage:read keys:read keys:write \
                              billing:read billing:write \
                              policy:read policy:write byok:read byok:write \
                              namespace:read"
```

### Auto-enable for the local daemon

When the credentials file is present, the local `bitrouter` daemon auto-adds the `bitrouter` provider to the in-memory zero-config providers map (see `apps/bitrouter/src/cloud/mod.rs::enable_in_zero_config`). Every model the account is entitled to becomes routable as `bitrouter:<model-id>` against `http://localhost:4356` — no `bitrouter.yaml` changes, no `BITROUTER_API_KEY` env var.

Inference credential precedence is explicit: when a target carries a key populated from `BITROUTER_API_KEY` (or an explicit config/override), that key is authoritative and the daemon does not read, refresh, or send requests for the stored OAuth credential. The stored login is the inference fallback only when the target has no key. Management commands under `bro cloud …` continue to use the stored account login independently; their identity does not override provider inference auth.

```python
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
client.chat.completions.create(
    model="bitrouter:openai/gpt-5.6-sol",  # pin the signed-in hosted provider
    messages=[{"role": "user", "content": "hi"}],
)
```

`bitrouter:<catalog-model-id>` pins a local request to the signed-in hosted
provider. Use the unpinned catalog ID, such as `openai/gpt-5.6-sol`, when the
normal local route-selection order should choose among all configured providers.

Cloud and BYOK coexist — providers with `${PROVIDER_API_KEY}` in the environment stay configured alongside the cloud provider.

### Manage the account from the terminal

After sign-in, typed `bro cloud …` subcommands cover common management operations:

```bash
bro cloud whoami                 # cloud base URL + local subject/scope
bro cloud usage                  # last 30 days of spend (micro-USD)
bro cloud keys list              # provisioned brk_… API keys
bro cloud keys mint --name laptop --scope "policy:read"
bro cloud billing balance        # credit balance
bro cloud policy list            # account-bound policies
bro cloud byok list              # BYOK provider keys
```

Every typed leaf accepts `--json` for raw response output. Use `bro cloud api <relative-endpoint>` for the rest of the Cloud `/v1/*` API surface, including public provider and usage discovery, settlement receipts, routing presets, OAuth clients, billing ledgers, checkout status, namespace lifecycle, and other account endpoints. See `references/cli.md` for the full subcommand index.

### Call Cloud APIs directly

The same stored credential drives a `gh api`-style raw client; the local daemon is not involved:

```bash
bro cloud api /v1/models
bro cloud api /v1/chat/completions --input request.json
bro cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

Use `-X` for the method, repeated `-H` headers, `-f` string fields, `-F` typed/nested fields, and `--input FILE|-` for exact body bytes. `--include`, `--silent`, and `--verbose` control output. Non-TTY/SSE bytes stream unchanged. Endpoints must be relative to the login origin and redirects are never followed.

### Sign out

```bash
bro cloud logout
```

Runs an RFC 7009 best-effort revoke at the AS, then deletes the local credentials file. Idempotent — re-running with no credentials present is a no-op.

## What Cloud does for the user that Local doesn't

- **No provider keys to manage.** One `brk_*` instead of `OPENAI_API_KEY` + `ANTHROPIC_API_KEY` + `GEMINI_API_KEY` + …
- **One bill.** A prepaid account balance with centralized billing instead of reconciling upstream invoices.
- **Curated registry.** Cloud uses a pinned public registry baked into the deploy image, so model availability and protocol routing match what the playground previews.
- **No daemon to keep alive.** No `bro status`, no `~/.bitrouter/`, no log rotation.

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

## D. Browser-approved Agent auth

This opt-in auth profile requires a cooperating Console. An unsupported server
fails visibly; it does not fall back to developer login or mint an API key.
Agent spending and provider/inference routing are separate rollout work.

```bash
bro cloud agent connect --profile laptop --name "Laptop Agent"
bro cloud agent status --profile laptop
bro cloud agent connect --profile laptop --name "Laptop Agent" --renew
bro cloud agent disconnect --profile laptop
```

Show the verification URL and pairing code to the owning human. They choose an
owned namespace, explicit finite daily limit and expiry in the browser. Approval
is followed by a separate client confirmation after durable credential storage.
Keep `--profile` to resume an interrupted operation; an omitted profile creates
an independent ID. Resume an interrupted registration with its original `--name`.
Ctrl-C detaches the waiter; it does not revoke the request.

Keys default to the OS key store. On supported Unix systems an explicit
`--key-storage file` fallback stores the key in a private profile. There is no
silent fallback. Tokens and pending state live under
`$XDG_DATA_HOME/bitrouter/agent-connections/`, or
`~/.local/share/bitrouter/agent-connections/`, outside the project. Directories
are mode 0700 and profiles mode 0600, with a cross-process lock and synced atomic
replacement. Platforms without qualified file-permission checks refuse before
storing credentials. Do not copy profiles or expose them to a model.

`status` can complete a previously approved pairing and rotate near-expiry
credentials. A lost rotation response stays `refresh_uncertain`; it does not
replay an old refresh token. Use fresh browser renewal with the same profile/key
or disconnect. Renewal retains the predecessor until confirmation succeeds.
Disconnect cancels the profile's native clients using their initiating key,
including before approval or after expiry/revocation; local secrets are erased
after server acknowledgement. Browser logout is independent.

Local MCP auth bridge (requires the `mcp` build feature):

```bash
bro cloud agent mcp
# Explicit headless file fallback, when the OS key store is unavailable:
bro cloud agent mcp --key-storage file
```

It exposes only `agent_connect`, `agent_status`, and `agent_disconnect`, using the
same library and profiles. Connect returns the human pairing information;
status saves/confirms after human approval. Tools return safe IDs/status, never
tokens, proofs, private keys or browser sessions. The issuer/resource/key-storage
settings belong to server startup, not model-supplied tool arguments.
Status and disconnect reject saved profiles for another startup issuer/resource
before network access or changes to saved connection state. This is a local
stdio auth bridge, not remote MCP OAuth or an automatically installed
origin MCP server. It does not change the daemon's inference credentials.

Self-hosted deployments explicitly set `--issuer` and `--resource` on connect or
MCP startup. Both must be trusted HTTPS authorities; the resource may include an
explicit path. Every native/control endpoint must remain on the configured
issuer. Redirects and legacy/API-key fallback are refused.
