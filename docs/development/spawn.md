# ACP launch, routing and cleanup

The command reference lives in [CLI](../CLI.md#bro-run--headless-agent).
This guide describes implementation ownership shared by headless execution and
ACP forwarding. Hidden compatibility command names are not separate runtimes.

## Entry points

`bro run` consumes the shared ACP client for a headless prompt.
`bro acp serve <agent>` forwards a configured external harness's ACP connection;
bare `bro acp serve` projects the daemon-owned native runtime. The latter does
not spawn itself through the external controller. Native vendor launchers retain
the vendor's own interaction and authentication flow. See the
[runtime guide](agent-runtime.md) for identity and authority ownership.

## Routing

External ACP routing is enabled by default; an explicit direct invocation opts
out. Model selection, explicit endpoint overrides, provider authentication and
process launch are different decisions. A native invocation instead uses the
local daemon and its native model selection. Never silently substitute a local
target when an explicitly selected endpoint or instance is unavailable.

Agent and runtime catalog entries describe reviewed process invocations and
routing injection. Apply only recognized rendering kinds; do not evaluate a
catalog as an arbitrary expression or construct shell commands from model
content. Temporary per-launch files and environment are owned by that launch,
not written into an unrelated vendor's permanent configuration. Source and
routing validation follow the [catalog guide](catalogs.md).

## Preflight and cleanup

Validate the selected harness, routing mode, daemon/auth requirements and
configuration before creating a session or launching a child. Failures are
reported through the command's supported structured error output or ACP error
path. A failed preflight cannot be presented as a successfully created session.
An explicit `--no-start` selection fails when its required daemon is absent;
activation and process creation do not precede that check.

One shared client/controller stack owns transport initialization, permissions,
cancellation and cleanup. A started process stays owned until confirmed cleanup;
losing an output stream or reaching a timeout does not prove that it stopped.
The harness's own ACP session ID is retained. Controller instance and launch
correlation are distinct from that session identity; an invented alias cannot
replace it in history, route operations or accounting.

Public native ACP and HTTP message formats are maintained in the
[service API](../specs/service-api/README.md). The CLI guide documents current
NDJSON output and hidden command compatibility. Startup observations and test
campaigns belong with their change, rather than in a second launch spec.
