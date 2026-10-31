# Remote administration

The CLI resolves an explicitly selected remote context to the remote endpoint
and token environment-variable reference. Remote inspection must use that
server's facts; it must not silently fall back to a local daemon, read the
client's files as remote policy state, or substitute local environment values.
The [CLI reference](../CLI.md#remote-contexts) gives commands and target flags.

## Shared actions and disclosure

Local and remote consumers reuse action identity, typed reports and the same
application-owned implementations. Capabilities describe enabled operations and
current scope, not an instruction to execute them. Passive inspection does not
run agent/MCP diagnostics, invoke providers, upload a configuration or perform a
reload. An explicit remote target that cannot be reached remains unavailable.

Reports expose safe operational categories, not credentials, arbitrary command
arguments, raw provider errors or local filesystem content. Policy reports
identify whether they describe active state or saved configuration. Missing
subsystems and storage errors remain distinguishable from an empty successful
report. Preserve the last successful observation as stale when appropriate;
never present stale state as a newly completed query.

## Reload and configuration state

Reload prepares the server's own saved source. It does not upload the client's
file or freeze an already reviewed immutable candidate. The owning coordinator
checks instance and generation, admits the operation and records participant
outcomes. HTTP disconnect does not cancel admitted work; partial application and
unknown outcomes remain explicit. Reload is not a transaction across every
subsystem. A client recovers the original operation rather than starting a new
one after a timeout.

[Configuration-state comparison](configuration-state.md) describes saved,
running and restart-required evidence. Authentication scopes, endpoint formats,
operation keys and retry behavior are defined once in the
[service API](../specs/service-api/README.md#control-api). Shared gateway
administration has its own actor and credential boundary within that same
specification; neither host-control nor inference tokens implicitly authorize it.
