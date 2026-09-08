# Remote-administration container acceptance

Run this from the repository root on a Linux/ARM64-capable Docker daemon:

```console
tests/remote-administration/run.sh
```

The default command is the complete acceptance gate. It includes a TLS MCP
client that initializes `/mcp-control`, obtains the runtime `tools/list`
inventory, and calls every discovered control tool with semantic assertions.
At the current workspace state, that default command correctly fails at MCP
initialization: the server rejects the sidecar's `Host: server:8443` header
with HTTP 403 before JSON-RPC begins. This is a live deployment finding, so the
MCP inventory and calls are not claimed as passed.

For diagnostics after that known MCP failure, the following selector runs the
independent CLI, HTTP, reload, and Code operations journeys:

```console
BITROUTER_REMOTE_ADMIN_SKIP_MCP=1 tests/remote-administration/run.sh
```

It prints `selected CLI/Code acceptance passed (MCP skipped)` on success.
It is not a substitute for the default acceptance gate.

The script builds `bitrouter-remote-administration:local` from the current
workspace unless `BITROUTER_REMOTE_ADMIN_SKIP_BUILD=1` is set. It removes every
container, network, certificate, and server-state directory it creates.

The topology deliberately has three network roles:

- The server container owns `bitrouter.yaml`, its policy lock, metering data,
  and a synthetic upstream credential. Its control API binds only to
  `127.0.0.1:4358`.
- A TLS-proxy sidecar shares the server network namespace and forwards TLS on
  `server:8443` to that loopback listener. The client cannot reach port 4358
  directly.
- Each client phase starts with an empty tmpfs home and `BITROUTER_HOME`. It
  receives only the test CA and the control-token environment variable it
  needs; it receives no server configuration or provider credentials.

The server’s loopback mock OpenAI endpoint makes actual inference requests so
the request inspection checks query the daemon’s own metering database. The
normal read phase invokes all nine advertised CLI leaves as well as their HTTP
routes. Once MCP initialization is accepted, it dynamically verifies the
advertised MCP control-tool inventory and invokes every tool through the same
TLS sidecar. It also runs a standalone synthetic local-trap subprocess: poisoned
config, metering, control-socket, and provider-environment inputs are watched
with Linux inotify and socket listeners while wrong-token and unreachable remote
reads run. The synthetic subprocess intentionally sets a fake `OPENAI_API_KEY`;
the normal client and actual server boundary still expose no server config or
provider credentials to the client. The fixture covers every advertised
`control:read` action, the `state` resource,
filtered and truncated request inspection, the TLS CLI path, reader denial of
reload, disk-versus-active policy divergence, an administrator reload after a
client connection closes, same-ID recovery, boot-instance change behavior, and
the legacy `BITROUTER_CONTROL_TOKEN` fallback after explicit credentials are
removed. That fallback remains read-only; the former named administrator token
is rejected after the restart. The recovery client reads the reload receipt's
`202` headers and closes before the operation body, then resolves that same
operation ID through the lookup endpoint.

The image also carries a separately compiled, ignored Rust test executable.
Its `docker_partial_fixture` is a test-only HTTP server that uses the real
control authentication, operation registry, and reload coordinator while
simulating a partial participant result. A second client phase confirms that
the CLI emits the `partially_applied` operation report and exits nonzero. Real
PTY checks drive the remote Code operations surface through the command palette
and temporary inspectors before a successful production reload, then through
the partial fixture. They verify models, requests, route, providers, telemetry,
agents, policy status/detail, reload state, route editing, explicit reload,
outcome rendering, generation changes, terminal restoration, and a subsequent
shell write. The
participant-level fault matrix remains in the Rust reload tests; the production
server container has no fault-injection switch. This is container acceptance,
not physical-host deployment verification.
