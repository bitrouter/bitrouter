# BitRouter Cloud SDK Consolidation Design

## Status

Approved for implementation on 2026-08-31. The implementation branch is
`codex/refactor-cloud-sdk`, based on `origin/main` commit
`9fa7a4c9973164bf2cb9b5f7339476b365888254`.

## Goal

Remove the `bitrouter-cloud-sdk` crate by moving each responsibility to the
smallest existing core crate or application module that owns it. Preserve one
BitRouter Cloud account credential source, make explicit API keys take
precedence over stored OAuth credentials, and keep refresh behavior safe under
concurrency.

## Non-goals

- Do not add multi-account support.
- Do not change the generic upstream-provider OAuth store or put BitRouter
  Cloud account credentials in it.
- Do not add Cloud APIs to `bitrouter-sdk`.
- Do not create a new shared Cloud client crate.
- Do not preserve public Rust APIs from `bitrouter-cloud-sdk`; breaking Rust
  API changes are accepted.
- Do not retain the historical behavior that passed an OAuth access token to
  settlement as an `x-api-key`.

## Current-state findings

The latest `main` update primarily changes routing optimization and the model
registry. The following Cloud-related areas are unchanged from the previously
reviewed base:

- `crates/bitrouter-cloud-sdk`
- `crates/bitrouter-providers/src`
- `apps/bitrouter/src/cloud`
- `crates/bitrouter-mcp/src/backend/cloud.rs`
- the workspace, application, provider, and MCP Cargo dependency declarations

No new `bitrouter_cloud_sdk` consumer was introduced. The removed optimization
evaluator was a former consumer of one Cloud setting, so the migration surface
is slightly smaller. `apps/bitrouter/src/main.rs` and
`apps/bitrouter/src/assemble.rs` remain important integration points even
though their Cloud behavior did not materially change.

## Ownership boundaries

### `bitrouter-providers`

`bitrouter-providers` owns reusable hosted-provider authentication. A new
optional `hosted` feature contains the BitRouter Cloud account credential
format, protected file store, OAuth device-flow and refresh primitives,
authorization-server metadata, credential manager, and the `bitrouter`
provider `AuthApplier`.

The intended module layout is:

```text
crates/bitrouter-providers/src/hosted/
├── mod.rs
├── applier.rs
└── account/
    ├── mod.rs
    ├── credentials.rs
    ├── manager.rs
    ├── flow.rs
    ├── metadata.rs
    └── settings.rs
```

Public modules do not re-export public items from sibling modules. Consumers
use the defining path, such as
`bitrouter_providers::hosted::account::manager::CredentialManager`.

The default feature set remains empty. The application enables both `pkce` and
`hosted`; downstream users that only need the provider catalog do not acquire
the hosted OAuth dependencies.

### `apps/bitrouter`

The application owns Cloud command behavior and Cloud-specific HTTP APIs:

```text
apps/bitrouter/src/cloud/
├── mod.rs
├── auth.rs
├── api.rs
├── api_client.rs
├── settlement.rs
└── management/
    ├── mod.rs
    ├── error.rs
    ├── billing.rs
    ├── budgets.rs
    ├── byok.rs
    ├── keys.rs
    ├── namespaces.rs
    ├── oauth_clients.rs
    ├── policies.rs
    ├── presets.rs
    ├── types.rs
    └── usage.rs
```

Login, logout, and `whoami` presentation live in `auth.rs`. The low-level raw
Cloud API and management clients are application-private implementation
details. Settlement remains application code because no reusable library
consumer needs it.

The telemetry adapter also remains in the application so
`bitrouter-providers` does not depend on `bitrouter-observe`.

### `bitrouter-mcp`

`bitrouter-mcp` defines the minimal billing response shape that its Cloud
backend consumes. It does not depend on an application module or on a new
shared Cloud crate.

### `bitrouter-sdk`

`bitrouter-sdk` is unchanged. Hosted account lifecycle and Cloud control-plane
types are not general routing SDK contracts.

## Credential storage and manager

There is exactly one persistent BitRouter Cloud account credential:

```text
<bitrouter-data-dir>/account-credentials.json
```

The generic upstream-provider store, `oauth-tokens.json`, continues to serve
providers such as GitHub Copilot, Claude Code, and OpenAI Codex. It never stores
the BitRouter Cloud account credential.

`CredentialManager` is path-parameterized and lazily reads the account file.
Construction must not parse the file or fetch metadata. The manager owns:

- the credential path;
- the refresh HTTP client;
- a metadata cache keyed by authorization-server origin;
- one asynchronous refresh gate shared by all in-process consumers.

The manager reloads the file after acquiring the refresh gate before using or
refreshing a stored credential. This lets separate processes observe login,
logout, or token rotation while ensuring that provider inference and telemetry
inside one process cannot run competing refresh-token exchanges.

Save and clear operations use the same gate. Writes remain atomic and
owner-only (`0600` on Unix) from initial temporary-file creation through the
final rename. The tagged API-key and OAuth representations remain readable,
and the legacy untagged OAuth representation remains compatible.

The manager exposes two resolution modes:

- bearer mode accepts an explicit API key, a stored API key, or stored OAuth;
- API-key-only mode accepts an explicit API key or stored API key and returns
  `WrongCredentialKind` for OAuth.

Both modes optionally require the stored credential origin to match a target
origin. Explicit API keys are target-scoped configuration and do not consult
the stored origin.

Manager errors are typed and distinguish at least:

- `NotSignedIn`;
- `Store`;
- `Metadata`;
- `Refresh`;
- `OriginMismatch`;
- `WrongCredentialKind`.

Callers map those errors according to their surface. The provider maps missing
or rejected credentials to an upstream 401, metadata discovery to an upstream
502, and malformed or unreadable local state to an internal error. Telemetry
degrades to anonymous attribution. CLI commands add command and path context.

## Credential precedence

The priority order is a hard invariant:

1. A request-specific raw `Authorization` header, where the request surface
   permits one.
2. A non-empty explicit BitRouter API key supplied by the routing target,
   configuration, `BITROUTER_API_KEY`, or the CLI-selected API-key environment
   variable.
3. The single credential in `account-credentials.json`.
4. Not signed in.

The explicit-key decision happens before opening or parsing the credential
file, acquiring the refresh gate, fetching authorization-server metadata, or
attempting refresh. Therefore a malformed stored OAuth file cannot mask an
explicit API key and explicit-key requests perform no OAuth network traffic.

`bitrouter cloud login --api-key` validates the key and atomically replaces the
current account credential. `bitrouter providers login bitrouter` invokes the
same login operation and writes the same file. The system never retains a
stored API key and stored OAuth credential simultaneously.

## Application data flow

### Daemon assembly

The application creates one `Arc<CredentialManager>` for the default account
path during assembly and passes clones to:

- the hosted provider `AuthApplier`;
- the account-attributed telemetry bearer adapter.

Neither consumer constructs or loads an independent credential store. The
provider passes the actual request origin to stored-credential resolution, so
a stored credential cannot be forwarded to an unrelated host. A raw
`Authorization` header is preserved before provider credential resolution.

Account telemetry receives the configured exporter origin when its bearer
adapter is built. Stored credentials are used only when that origin matches;
resolution failures remain best-effort and produce anonymous telemetry.
Explicit telemetry tokens stay on the existing static-token path and do not
read the account store.

Standalone ACP surfaces create one manager for their process and apply the
same precedence. A configured or environment API key wins before signed-in
account fallback.

### Cloud CLI and management APIs

Each Cloud CLI invocation creates one manager and passes it through login,
logout, identity, raw API, and management-client construction as needed.
Commands do not instantiate `CredentialsStore` directly.

OAuth login discovers metadata, performs device authorization, then saves
through the manager. API-key login validates the key and saves through the same
manager. Logout attempts OAuth revocation on a best-effort basis before
clearing the file; API-key logout is local-only. `whoami` remains local and
never refreshes or prints secret values.

### Settlement

Settlement only sends `x-api-key`. Its resolution order is:

1. a non-empty key from the environment variable selected by `--api-key-env`;
2. a static API key from `--credentials-file`;
3. an explicit error.

An environment key bypasses the credential file completely, including a
missing, malformed, or OAuth-only file. A credential-file API key must match
the `--api-base` origin. An OAuth credential produces `WrongCredentialKind`;
it is never refreshed or placed in `x-api-key`.

This intentionally tightens the existing `reconcile-metering` behavior and
its documentation.

## Security properties

- Secret-bearing `Debug` implementations redact API keys, access tokens, and
  refresh tokens.
- Stored credentials are origin-confined before use outside an explicitly
  configured target key.
- Refresh-token rotation is single-flight within a process and persisted
  atomically before the refreshed access token is returned.
- Explicit API keys bypass all stored OAuth state and metadata calls.
- Telemetry never turns authentication failure into daemon startup or export
  failure.
- Settlement cannot coerce OAuth credentials into an API-key header.
- OSS source comments describe public protocols and behavior only; migrated
  code does not reference private Cloud implementation modules.

## Migration sequence

1. Add the `hosted` feature, account modules, credential manager, and hosted
   provider applier to `bitrouter-providers` while the old crate still exists.
2. Switch app authentication, provider registration, telemetry, ACP, login,
   management, metering, workflow-state, and archive consumers to the new
   ownership boundaries.
3. Replace the MCP dependency with its local billing wire type.
4. Move settlement into the application and enforce API-key-only resolution.
5. Delete `bitrouter-cloud-sdk` and remove workspace, lockfile, release, source,
   test, and active-documentation references.

Each migration stage must compile and retain focused tests before the next
stage removes the compatibility source.

## Testing and acceptance

Focused tests must prove:

- legacy untagged OAuth, tagged OAuth, and tagged API-key JSON load correctly;
- saves are atomic and owner-only on Unix;
- API-key login replaces OAuth in the single account file;
- provider and telemetry consumers sharing one manager issue one refresh under
  concurrent near-expiry access and persist the rotated token;
- an explicit API key succeeds without reading a malformed credential file or
  fetching metadata;
- a raw request `Authorization` header is not overwritten;
- stored credentials are rejected on origin mismatch;
- telemetry failures degrade to anonymous attribution;
- settlement chooses an explicit environment API key before a credential file;
- settlement accepts a matching stored API key and rejects OAuth;
- the provider-login alias and Cloud login use the same manager path;
- `bitrouter-providers` compiles with default features and with `hosted`.

Repository-wide acceptance requires:

```text
cargo check -p bitrouter-providers
cargo test -p bitrouter-providers --features hosted
cargo nextest run --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

Local wiremock tests run with HTTP proxy environment variables unset so local
requests are not redirected through a proxy.

Completion also requires searches proving that:

- `bitrouter-cloud-sdk` is absent from workspace manifests, `Cargo.lock`, and
  release configuration;
- live Rust source and active documentation contain no
  `bitrouter_cloud_sdk` references;
- the deleted crate directory is absent;
- historical changelog entries may retain the old released crate name only as
  historical record.
