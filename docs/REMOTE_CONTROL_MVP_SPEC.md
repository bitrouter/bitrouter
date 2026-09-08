# Spec: remote control MVP (HTTP, no remote ACP)

> Code presentation update (2026-09-08):
> [CODE_TUI_UX_SPEC.md](CODE_TUI_UX_SPEC.md) supersedes permanent dashboard
> navigation and independent interactive drivers. It preserves public CLI names,
> native-session ownership, typed action boundaries, and read-only remote scope.
> [Implementation verification](CODE_TUI_UX_PROGRESS.md) is tracked separately.

Status: **implemented and verified.**
Baseline: `main` at `61dd77333a91f7aab01647b0ae7d8625d3039ffe`.

## Decision

The first remote release moves BitRouter's read-only operational controls over
authenticated HTTP. It does **not** carry ACP sessions over the network.

The proxy and every ACP agent stay together on the server machine. A client on
another machine can inspect that BitRouter instance with the headless CLI or
the interactive TUI. Remote agent work uses SSH (`ssh -t host bitrouter tui
<agent>`) until the separate ACP transport proposal is stable enough to
implement.

This leaves two user-facing interaction modes:

- the CLI for composable, machine-readable actions;
- the TUI for interactive exploration of those same actions.

`agents`, `spawn`, `acp`, and `launch` describe resources or integration
mechanisms. They are not a third presentation surface.

## MVP contract

The daemon may expose a dedicated control listener:

```yaml
control:
  enabled: true
  listen: 127.0.0.1:4358
```

The listener is disabled by default, must bind a loopback address, and requires
an operator token from `BITROUTER_CONTROL_TOKEN`. The token is never stored in
`bitrouter.yaml`; clients refer to the environment variable that contains it.
Inference `server.skip_auth` has no effect on control authentication.

Operators expose the loopback listener through an authenticated private tunnel
or a TLS reverse proxy. BitRouter does not ship TLS termination, browser CORS,
cookies, or a public-internet bind in this MVP.

Every route is under `/control/v1`:

| Method | Path | Action |
| --- | --- | --- |
| `GET` | `/capabilities` | Protocol/version handshake and supported actions |
| `GET` | `/status` | Daemon health and host-wide spend |
| `GET` | `/models?provider=<id>` | Routable model catalog |
| `POST` | `/route/preview` | Read-only route resolution |
| `GET` | `/requests?limit=<n>` | Recent settled requests |

The implementation ships capabilities, status, models, route preview, bounded
recent requests, and the dashboard on the same versioned transport.

Responses reuse the existing typed action reports. Errors use:

```json
{
  "error": {
    "code": "unauthorized",
    "message": "valid control bearer token required"
  }
}
```

The server never returns provider credentials, API keys, environment values,
config contents, or its control-socket path. There is no mutation endpoint.

## Client behavior

Remote selection is orthogonal to commands. The target is a named context:

```console
bitrouter context add workstation \
  --endpoint https://router.example/control/v1 \
  --token-env WORKSTATION_BITROUTER_TOKEN
bitrouter --context workstation status
bitrouter --context workstation models
bitrouter --context workstation route openai/gpt-5
bitrouter --context workstation tui
```

The local context continues to use the Unix socket, local config, and local
metering database. A remote context uses HTTP only and never falls back to the
client machine's config or database. Network/auth/version failures are errors;
they are never rewritten as `running: false`.

The capability handshake runs before the requested action. Clients reject an
incompatible protocol version and report unsupported actions explicitly.

Remote `start`, `stop`, `restart`, `reload`, config, key/provider management,
agent lifecycle, ACP sessions, route mutations, and harness launches are out of
scope. A remote context supplied to one of them fails before performing a local
side effect.

## Security invariants

- The control listener is opt-in and loopback-only.
- A bearer token of at least 32 bytes is required at daemon startup.
- Token comparison is constant-time.
- `server.skip_auth` cannot disable control authentication.
- Credentials stay out of config, reports, errors, and logs.
- HTTP redirects are not followed by the client, so credentials cannot be
  forwarded to another origin.
- The API is read-only and has no browser CORS or cookie authentication.
- Status and requests are explicitly host-wide operator data in this trusted,
  single-operator MVP.

## Delivery order

1. Config, authenticated listener, capability handshake,
   status/models/route/requests, and server tests.
2. HTTP action client plus explicit endpoint targeting.
3. Named contexts with credential references and no-local-fallback tests.
4. General `bitrouter tui` home/dashboard backed by the same action client.
5. Packaging, tunnel/reverse-proxy examples, compatibility tests, and threat
   review.

Remote ACP/WSS is intentionally deferred to
[`REMOTE_CLI_TUI_SUPPORT_SPEC.md`](REMOTE_CLI_TUI_SUPPORT_SPEC.md), now treated
as a Phase 2 RFD rather than part of this MVP.
