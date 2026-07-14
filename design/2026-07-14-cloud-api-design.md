# BitRouter Cloud API Command Design

**Date:** 2026-07-14

**Status:** Approved for implementation

## Summary

Add a general-purpose `bitrouter cloud api <endpoint>` command modeled after
[`gh api`](https://cli.github.com/manual/gh_api). It sends authenticated HTTP
requests to the BitRouter Cloud host recorded by `bitrouter cloud login`, while
remaining useful for streaming model-generation APIs. In the same change, add
`bitrouter cloud login --api-key <brk_api_key>` for non-interactive CI and
automation environments.

The initial documented and tested endpoint set is:

- `GET /v1/models`
- `POST /v1/chat/completions`
- `POST /v1/messages`
- `POST /v1/responses`
- `POST /v1beta/models/{model}:generateContent`
- `POST /v1beta/models/{model}:streamGenerateContent`

The endpoint argument is not an allowlist. Any relative API path on the stored
BitRouter Cloud origin is accepted, so new API endpoints do not require a CLI
release.

## Goals

1. Give users a familiar authenticated raw-API command with the core request
   ergonomics of `gh api`.
2. Reuse the credential established by `bitrouter cloud login` for API calls,
   the built-in cloud provider, and existing typed cloud-management commands.
3. Support both OAuth device-flow credentials and static `brk_` API keys as
   first-class stored credential types.
4. Preserve streaming behavior for all supported model-generation protocols.
5. Ship comprehensive automated tests and English/Chinese documentation.

## Non-goals

The first release does not implement GitHub-specific or currently unnecessary
`gh api` features:

- GitHub repository placeholders, previews, GraphQL, or host selection
- pagination and slurping
- response caching
- `--jq` and Go templates

Users can pipe JSON output to an installed `jq`. Generation requests are not
safe cache candidates, and the initial endpoint set does not paginate.

## Considered Approaches

### 1. General relative-path API client (selected)

Expose one `api <endpoint>` command and implement request construction,
authentication, response streaming, and output controls once. This matches the
mental model of `gh api` and automatically accommodates future endpoints.

### 2. Typed endpoint subcommands

Expose `api models`, `api chat-completion`, `api messages`, and similar
subcommands. This provides compile-time request types but duplicates upstream
wire schemas, lags new API fields, and does not meet the `gh api` design goal.

### 3. Typed subcommands plus a raw escape hatch

Expose both forms. This is flexible but creates two overlapping public
interfaces and substantially expands the first release without adding a user
capability beyond the general client.

## CLI Surface

```text
bitrouter cloud api <endpoint> [flags]

  -X, --method <METHOD>
  -H, --header <KEY:VALUE>
  -f, --raw-field <KEY=VALUE>
  -F, --field <KEY=VALUE>
      --input <FILE|->
  -i, --include
      --silent
      --verbose
```

Examples:

```bash
bitrouter cloud api /v1/models

bitrouter cloud api /v1/chat/completions --input request.json

bitrouter cloud api /v1/messages \
  -F model=anthropic/claude-sonnet-4-5 \
  -F max_tokens=256 \
  -f 'messages[][role]=user' \
  -f 'messages[][content]=Hello'

bitrouter cloud api /v1/responses --input - <<'JSON'
{"model":"openai/gpt-5","input":"Hello"}
JSON

bitrouter cloud api \
  '/v1beta/models/google/gemini-2.5-flash:streamGenerateContent' \
  --input request.json
```

### Method selection

- The default method is `GET` when there are no fields and no input body.
- Supplying `-f`, `-F`, or `--input` changes the implicit method to `POST`.
- An explicit `-X/--method` always wins.
- With `-X GET`, fields become query parameters.
- With `--input`, the file or stdin is the request body and any `-f`/`-F`
  fields become query parameters, matching `gh api`.

### Fields

`-f/--raw-field` always produces a JSON string. `-F/--field` converts the
literals `true`, `false`, `null`, and signed integer values to their JSON types.
A typed value beginning with `@` reads the remainder from a UTF-8 file; `@-`
reads stdin.

Both field forms support the `gh api` nested-key grammar:

- `key[subkey]=value` creates nested objects.
- Repeated `key[]=value` entries create arrays.
- `key[]` creates an empty array.
- Repeated object-array keys such as `messages[][role]` and
  `messages[][content]` populate objects in insertion order.

Malformed fields, incompatible object/array reuse, unreadable files, and more
than one attempt to consume stdin fail before the HTTP request is sent.

### Headers and request bodies

The command supplies these defaults unless the user overrides them:

- `Authorization: Bearer <stored credential>`
- `User-Agent: bitrouter/<version>`
- `Accept: application/json`
- `Content-Type: application/json` when the command builds a JSON body

`--input` sends the input bytes without parsing or rewriting them. The user may
set `Content-Type` with `-H` for a non-JSON payload. Header names are parsed
case-insensitively, invalid header syntax is rejected, and repeated headers
follow normal HTTP combination semantics. User-provided `Authorization` is
allowed, but the destination remains confined to the stored origin.

## Endpoint Resolution and Origin Confinement

The OAuth authorization-server URL or API-key base URL recorded at login is the
only origin the command may contact.

- `/v1/models` and `v1/models` both resolve against that origin.
- Existing endpoint query parameters are preserved and may be extended by
  field-derived query parameters.
- Absolute URLs, scheme-relative URLs such as `//example.com/path`, URL user
  information, and fragments are rejected.
- Redirect following is disabled for `cloud api`, preventing a trusted origin
  from redirecting the bearer to another host.

These rules apply even when the caller supplies an `Authorization` header. The
command is an API client for the logged-in BitRouter Cloud deployment, not a
general authenticated `curl` replacement.

## Stored Credential Model

Replace the OAuth-only in-memory model with a tagged stored credential enum:

```rust
enum StoredCredential {
    Oauth(OauthCredential),
    ApiKey(ApiKeyCredential),
}
```

The persisted forms are conceptually:

```json
{
  "kind": "oauth",
  "access_token": "...",
  "refresh_token": "...",
  "expires_at": "...",
  "token_type": "Bearer",
  "scope": "...",
  "client_id": "bitrouter-cli",
  "authorization_server": "https://api.bitrouter.ai",
  "namespace_id": "...",
  "subject": "..."
}
```

```json
{
  "kind": "api_key",
  "api_key": "brk_...",
  "base_url": "https://api.bitrouter.ai"
}
```

Deserialization accepts the existing untagged OAuth object as a legacy form,
so upgrades do not log users out. The next OAuth refresh or login writes the
tagged form. `Debug` implementations redact OAuth tokens and API keys.

The credential store retains its current properties:

- a single current credential per BitRouter data directory
- atomic sibling-file replacement
- mode `0600` from file creation on Unix
- no credential value in normal logs or command output

Shared credential accessors provide the stored origin, authentication kind,
namespace display value, and current bearer. OAuth bearer resolution keeps
metadata discovery, the 60-second refresh window, refresh-token rotation, and
single-flight refresh behavior. API-key bearer resolution returns the static
key without metadata discovery or network traffic.

## API-Key Login

```text
bitrouter cloud login --api-key <brk_api_key> [--oauth-as <URL>]
```

- Without `--api-key`, login remains the existing OAuth device flow.
- With `--api-key`, login validates the `brk_<token_id>.<secret>` shape and
  saves an API-key credential without making a validation request.
- `--oauth-as` supplies the API base origin for self-hosted deployments and
  otherwise defaults through the existing flag/environment/default resolution
  chain.
- `--api-key` conflicts with OAuth-only `--client-id` and `--scope`.
- Login never prints the key. Documentation uses
  `--api-key "$BITROUTER_API_KEY"` and warns that literal command-line secrets
  can be retained in shell history and visible to process inspection.

`bitrouter cloud whoami` reports `authentication: oauth` or
`authentication: api_key`. OAuth mode continues to show safe metadata such as
scope, subject, namespace, and expiry. API-key mode shows only the base URL and
credentials path.

`bitrouter cloud logout` revokes OAuth tokens on a best-effort basis before
removing the file. API-key logout performs no network request and only removes
the local credential.

## Existing Credential Consumers

All current consumers use the new shared credential abstraction:

1. `bitrouter cloud api` uses the stored bearer and origin.
2. The built-in cloud provider uses OAuth or the stored API key before falling
   back to an inline `BITROUTER_API_KEY` routing-target key.
3. Typed management commands use OAuth or API-key bearers. When an API-key
   credential has no locally known namespace id, namespace-scoped paths use
   `/v1/namespaces/me/...`, which the service resolves from the credential.
4. Zero-config cloud-provider activation continues to key off the presence of
   a readable credential file.
5. Telemetry bearer resolution accepts either stored credential type without
   logging it.

An API key can only perform operations allowed by its server-side scopes.
Missing-scope responses keep the existing actionable error handling, except
that API-key users are told to mint or select a key with the required scope
rather than rerun OAuth login.

## HTTP Execution and Output

Request construction and HTTP execution live in `bitrouter-cloud-sdk`; Clap
parsing and terminal I/O orchestration live in the `bitrouter` app. The SDK
returns status, headers, and a streaming response body instead of forcing JSON
deserialization.

- `application/json` and `+json` bodies are colorized/indented only when stdout
  is an interactive terminal; otherwise bytes are preserved for pipelines.
- `text/event-stream` and all other content types are copied incrementally to
  stdout without buffering the full response.
- `--include` writes the HTTP version/status line and sorted response headers
  before the body.
- `--silent` consumes the response but suppresses the body. It does not turn an
  HTTP error into success.
- `--verbose` writes request/response diagnostics to stderr. Authorization,
  API-key headers, and other credential-shaped values are replaced with
  `<redacted>`.
- HTTP status `>= 400` prints the response body through the selected normal
  output path and returns a non-zero CLI exit. Transport and local validation
  errors use the existing BitRouter error-reporting path.
- A body ending without a newline is not rewritten. SSE event framing is
  therefore preserved exactly.

## Component Boundaries

The implementation should keep the existing large cloud CLI module from
growing further:

- `crates/bitrouter-cloud-sdk/src/auth/credentials.rs`: stored credential
  variants, compatibility deserialization, persistence, and current bearer.
- `crates/bitrouter-cloud-sdk/src/api.rs`: origin-confined raw request client
  and streaming response type.
- `apps/bitrouter/src/cloud/api.rs`: Clap args, field grammar, request assembly,
  and output writer behavior.
- `apps/bitrouter/src/cloud/cli.rs`: adds `CloudAction::Api`, extends login
  flags, and dispatches to the focused modules.
- `apps/bitrouter/src/cloud/mod.rs` and management/provider modules: consume the
  shared credential abstraction.

Exact file decomposition may adjust during the implementation plan if the
latest source layout provides a smaller boundary, but credential semantics and
public CLI behavior are fixed by this design.

## Testing Strategy

Implementation follows red-green-refactor cycles.

### Credential tests

- round-trip tagged OAuth and API-key credentials
- load the legacy untagged OAuth file
- redact every secret from `Debug`
- preserve atomic writes and Unix `0600` permissions
- validate API-key shape and CLI flag conflicts
- return API keys without metadata or refresh calls
- preserve OAuth refresh and rotation behavior
- skip revocation for API-key logout
- render safe `whoami` output for both modes

### Request-construction tests

- relative-path resolution and query preservation
- reject absolute, scheme-relative, fragment-bearing, and user-info endpoints
- disable cross-origin redirects
- default/explicit method selection
- header defaults, overrides, repeats, and validation
- raw versus typed field conversion
- nested object/array field grammar and malformed conflicts
- file/stdin typed values and request bodies
- fields become query parameters with `GET` or `--input`
- prevent conflicting multiple stdin consumers

### Wire-level tests

Use a mock HTTP server to assert bearer headers, path, method, headers, query,
and body for:

- `/v1/models`
- `/v1/chat/completions`
- `/v1/messages`
- `/v1/responses`
- `:generateContent`
- `:streamGenerateContent`

Additional cases cover JSON, SSE split across chunks, non-JSON bodies,
`--include`, `--silent`, `--verbose` redaction, non-2xx responses, and both
OAuth/API-key stored credentials. Existing provider and management test suites
remain regression coverage for all previous consumers.

### CLI and full-suite verification

- Clap parsing tests for the new action and login conflicts
- binary-level tests with an isolated `XDG_DATA_HOME`
- `cargo test --all-features` (or `cargo nextest run --all-features`)
- `cargo clippy --all-features`
- `cargo fmt -- --check`
- documentation/link checks required by the repository

## Documentation

The same change updates:

- `CLI.md` command reference
- the shipped BitRouter Agent Skill CLI and cloud-setup references
- English and Simplified-Chinese product docs in lockstep
- a dedicated Cloud API guide with copyable examples for models, Chat
  Completions, Messages, Responses, Generate Content, and streaming
- CI login guidance, API-key security warning, authentication precedence,
  supported flags, and explicit first-release omissions

Product documentation examples use environment-variable expansion for API-key
login and never contain a plausible real credential.

## Acceptance Criteria

1. A fresh OAuth login can call all initial endpoint families through
   `bitrouter cloud api`.
2. `bitrouter cloud login --api-key "$BITROUTER_API_KEY"` works without a TTY
   or network validation and the stored key drives the same endpoints.
3. Streaming responses reach stdout incrementally and retain exact event
   framing.
4. Existing provider, telemetry, and typed management consumers work with
   OAuth and stored API-key credentials as applicable.
5. Existing credential files load without migration steps or re-login.
6. Secrets are absent from normal, debug, error, and verbose output.
7. Requests cannot send stored credentials to a different origin, including by
   redirect.
8. The documented flags and five API families have automated success and
   failure coverage.
9. English/Chinese product docs and the shipped Agent Skill match the actual
   CLI.
10. The complete repository verification suite passes.
