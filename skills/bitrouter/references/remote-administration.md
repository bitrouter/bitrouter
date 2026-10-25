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

`control:tenancy` is a separate installation-management grant. It does not imply
`control:read` or `control:reload`; use a dedicated credential when only tenant
management is needed. The optional `tenancy.enabled: true` service requires
authenticated ingress, this explicit operator grant, local gateway state and a
literal SQLite file URL. Its initial owning-process profile is qualified on Unix;
shared databases, memory databases, encoded paths and the host coding-task API
are rejected. Enabling or disabling tenancy requires restart. Once provisioned,
the store cannot be reopened by ordinary local database commands with tenancy
disabled; use protected HTTP administration. Read-only host reports remain
available. Stop all old writers before first enabling tenancy, especially older
binaries that predate ownership leases. Current writable pools retain a shared
file lease, and the authority owner requires an exclusive lease until its
transactions and connections have settled. `mode=rw` never creates a missing
database.

Tenancy operator credential IDs are limited to 119 bytes. Their stable principal
ID is exactly `operator.<configured ID>`; use
`PrincipalId::for_control_credential` to derive the management owner for binding
updates. Token rotation preserves this identity; renaming the configured ID
changes it. Ordinary principals cannot use the reserved `operator.` prefix.

`tenancy.resource_import` optionally imports an operator JSON resource manifest
at startup; relative paths use the server process working directory. Set
`tenancy.wrapping_key_env` to an environment variable holding a stable base64
32-byte key. Import requires that key, and retained resources require the same
key on later restarts even after removing the import path. These settings are
restart-only. Never treat a replacement key as automatic rotation. The importer
accepts explicit route/policy definitions and static API-key credential slots;
OAuth/ambient backends and native request checks are not yet supported. Definitions
are retained as authenticated ciphertext, and a changed definition requires a
new qualified resource revision. Importing resources does not grant tenant access
or activate a configuration. Never put resource-import definitions or secret key
values into a tenant draft; drafts contain resource references only.

Use the HTTP `TenancyClient` for tenant management or the `bro tenancy`
configuration adapter below. The tenant-operation namespace
is `/control/v1/tenancy/operations/{operation_id}`, separate from host reload
operations. Retain operation IDs after a lost response; credential secrets are
returned only once. Authority-only servers keep dispatch, revision binding and
strict receipts disabled, and inference is fenced until those runtime stages are
implemented. Do not send tenant inference based solely on successful credential
issuance or configuration administration.

Controlled native turns use the same tenant credential and dispatch binding as
HTTP turns. Caller labels, host inspection credentials and local harness metadata
cannot bypass the dispatch capability fence. Historical evaluation rows remain
legacy records after schema upgrades; enabling tenancy does not assign them to a
tenant. Signed experiments also require qualified tenant exploration admission;
an Observe configuration does not authorize an active experiment.

## Tenant configuration commands

Create a dedicated named context using the selected installation operator or
tenant credential's environment variable. A tenancy-only operator token does not
need `control:read` for these commands. Each remote command can instead take an
explicit `--endpoint URL --token-env NAME` pair. Targets cannot be mixed, and
there is no `--context local` or implicit local discovery.

```console
bro context add resource-admin --endpoint https://router.example/control/v1 --token-env RESOURCE_ADMIN_TOKEN
bro --context resource-admin tenancy capabilities
bro tenancy configuration validate --input draft.json
bro --context resource-admin tenancy configuration apply --tenant tenant-a --input draft.json
bro --context resource-admin tenancy configuration read --installation-id install-a --tenant tenant-a --revision config-r1
bro --context resource-admin tenancy configuration active --installation-id install-a --tenant tenant-a
bro --context resource-admin tenancy configuration activate --tenant tenant-a --input activation.json
bro --context resource-admin tenancy operation --installation-id install-a --operation-id prepare-1
```

`draft.json` must be the complete `ConfigurationPut` request: a caller-chosen
stable `operation_id`, actual `installation_id`, observed
`expected_authority_revision`, and typed `document`. The same file is used for
local validation and apply. `activation.json` is a complete `Activation` request
with a separate stable operation ID, installation, observed authority revision,
`expected_active_revision`, and retained target `revision_id`. The active CAS
field is required even when its value is `null`. Resource definitions must
already be provisioned; apply sends references and framework settings only.

Validation is explicitly local shape/bounds checking. It reads no context or
credential and cannot confirm authorization, resource existence, compilation,
CAS, or dispatch readiness. Files are literal JSON, bounded to 2 MiB with a
1 MiB configuration limit. They do not expand environment variables. Apply
prepares a revision; activation is always explicit. Active-pointer reads require
tenant-wide resource.read or installation-operator authority.

The adapter performs one SDK request per remote action, without a host-read
handshake, automatic retry, identity generation, or CAS substitution. After an
uncertain mutation, query its original installation/operation using the same
principal and current read authority. Lookup absence is not proof of rollback.
When an applied configuration operation returns its retained revision, explicitly
read that revision. Never create a replacement operation ID just to resolve a
lost response. Non-applied operation reports exit nonzero but retain their single
JSON result; `--human` provides the corresponding human view.

Capability discovery succeeds even if dispatch is disabled. Require all three
dispatch flags before strict inference; these client commands do not turn them on.

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

## Saved versus running configuration

`status.config_state` is computed by the target daemon, including independently
loaded policy-lock and access-policy evidence. Missing/invalid files, unknown
older-daemon capabilities, partial reloads, and restart-only changes are explicit;
clients never substitute local config or credentials. Local IPC, SIGHUP, and
remote reload now share startup-field rejection rules. Read `running` and the
safe changed-field categories before choosing reload or restart; file writes
alone do not establish activation.
