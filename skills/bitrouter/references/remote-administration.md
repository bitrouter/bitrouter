# Remote router administration

Use `--context NAME` to select the router host before choosing an action.
Inference endpoints (`--base-url`) do not select a control host or move agents.
The client needs only a named endpoint and its control-token environment variable.
Provider credentials, router config, and metering stay on the server.

## Host credentials

Keep the listener on loopback behind an existing private tunnel or TLS proxy:

```yaml
control:
  enabled: true
  listen: 127.0.0.1:4358
  credentials:
    - id: observer
      token_env: ROUTER_OBSERVER_TOKEN
      scopes: [control:read]
    - id: operator
      token_env: ROUTER_OPERATOR_TOKEN
      scopes: [control:read, control:reload]
```

Set distinct bearer tokens of at least 32 bytes in the host environment before
starting the service. Config stores variable names, never token values. Changing
control credentials or listener configuration requires a daemon restart.

With no explicit credentials, `BITROUTER_CONTROL_TOKEN` remains the required
legacy credential and receives only `control:read`. With explicit entries,
only those entries authenticate; the legacy variable is not an extra credential.
Read authority includes host-wide spend, request metadata, and policy rules.
It is not a personal usage scope. Inference `server.skip_auth` has no effect.

## Client context

Set the selected token in the client's environment, then save its variable name:

```console
bro context add workstation \
  --endpoint https://router.example/control/v1 \
  --token-env WORKSTATION_CONTROL_TOKEN
bro --context workstation status
bro --context workstation models
bro --context workstation code
```

Plain HTTP is accepted only for a loopback URL, such as an SSH port-forward.
Redirects are rejected. Do not combine a remote context with local `--config`
or `--socket`. Authentication, capability, and transport errors never fall back
to the client's router configuration or database.

Capabilities determine what the selected server can answer. Older servers can
still provide their existing reads; upgrading a client does not enable actions
the server does not advertise. Remote ACP and native harness execution remain
unavailable. Agent checks, MCP diagnostics, login, config editing, and service
start/stop/restart continue to require host-local execution.

## Inspect the host

```console
bro --context workstation providers list
bro --context workstation observe status
bro --context workstation agents list
bro --context workstation policy status
bro --context workstation policy show production --view active
bro --context workstation policy status --view disk
bro --context workstation requests --limit 50 --provider openai \
  --since 2026-09-07T00:00:00Z --until 2026-09-08T00:00:00Z
```

Remote policy reads default to `active`; local policy reads retain their disk
view unless `--view active` is explicit. The disk view shows host files waiting
to be applied. Agent listing is passive and never starts an agent. Remote
`agents list --remote` is rejected because that flag selects an external registry.

Request intervals require both RFC3339 bounds, span at most seven days, and use
an inclusive start and exclusive end. Limits are 1–500 rows. Summary filters
apply before the row limit; `truncated` describes omitted matching rows. Metering
availability distinguishes missing storage from an available empty report.
Rate counters remain host-wide and report that scope explicitly.

## Reload and recover

Prepare configuration using the host's existing deployment workflow, inspect
its disk policy, then use a credential with `control:reload`:

```console
bro --context workstation reload
bro --context workstation operations show REQUEST_UUID --instance BOOT_UUID
```

Reload reads only server-owned files and does not forward client environment
values. Preparation has a 60-second deadline; a live commit is never interrupted
to meet a client timeout. Listener, credentials, and other startup-only changes return explicit
restart-required fields before applying any change. Each result identifies the
participants that applied, failed, remained unchanged, or were not attempted.
A partially applied result is not a rollback; inspect the active policy and
runtime state before deciding the next host-side change.

The client submits one operation and waits up to 30 seconds for its result. If
it reports `running` or loses the connection, retain the request and boot IDs
and query `operations show`; never blindly issue another reload. The daemon
continues an admitted operation after HTTP disconnect. Lookup requires the same
credential, and results remain in memory for at least 24 hours after completion,
with a maximum of 1,024 retained operations. A restart or expired/missing result
means the outcome is unknown, not that the earlier reload did not happen.
