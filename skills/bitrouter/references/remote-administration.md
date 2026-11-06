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
literal SQLite file URL. This profile supports one owning process on Unix;
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

Execution lifetime is configured by the host at startup:

```yaml
tenancy:
  enabled: true
  ordered_attempt_limit: 8
  submission_timeout_secs: 60
  execution_timeout_secs: null
  stream_idle_timeout_secs: 120
  resource_origins: []
```

`tenancy.capacity` selects finite process budgets independently of stored tenant
and key counts. Omission preserves the lightweight defaults. For example, a
single-instance operator can select an active execution budget without changing
cache or upload bytes:

```yaml
tenancy:
  enabled: true
  capacity:
    execution:
      instance_active: 512
      tenant_active: 32
      instance_queue: 1024
      tenant_queue: 64
```

This requires the same authenticated control and exclusive SQLite profile as
other tenancy settings. Increasing a budget does not override a request's
permissions or public protocol limits. Queues may be zero to disable waiting;
other counts and bytes must be positive. Capacity changes require restart.
Choose budgets using the deployment's measured memory, disk and latency, and
keep explicit room for maintenance. See the canonical configuration specification
for all fields and the distinction between runtime and retained directory size.

Authority-view memory is allocated lazily: one third of its byte budget holds
leased and optionally retained facts and two thirds cover publication staging.
Directory entries are not preallocated. Pressure releases optional retention
before refusing allocation; active leases keep their ownership. Raising directory counts
does not enlarge caches, queues or connection pools. Retained unknown obligations
remain valid when admission budgets are lowered; new work must fit the new
budgets. There is no fixed tenant/key-count ceiling. The
[process-capacity contract](../../../docs/specs/configuration/README.md#process-capacity)
lists defaults and machine/budget validation rules; public request limits still
apply when host budgets are raised.

`receipts.high_water_bytes` covers live receipt accounting and pending completion
reservations, not all retained history or the SQLite file. Eligible terminal
payloads move to the database's internal archive while IDs and authorized reads
remain available. Unknown obligations and live owners cannot be archived away.
History/OAuth byte estimates and actual volume free space are separate facts;
archive compression does not prove space was returned to the filesystem.
Monitor real disk/WAL growth and maintain headroom: other filesystem users and
SQLite write amplification can still cause ENOSPC after an admission probe.

The format-2 store has no lifetime directory/history count quota. Startup walks
all retained pages before serving; an insufficient validation working budget
never authorizes a partial scan. Healthy legacy stores upgrade during validated
startup. A verified legacy count-saturation latch requires the explicit stopped
`tenancy offline upgrade-capacity` procedure in
[the CLI reference](cli.md#local-db-only-offline-evidence); it is not a general
fence-clearing command. Do not delete history or choose new operation IDs to
work around uncertain outcomes.

Submission bounds upload and admission, not the lifetime of an admitted stream.
`ordered_attempt_limit` bounds explicit ordered v2 submissions to 1–64 candidates
(default 8). Hosts serving longer reviewed fallback chains can opt into 64;
ordinary v1 submissions keep their eight-attempt bound. This work budget does
not limit the number of tenants or retained routes. An oversized selection is
rejected before dispatch rather than truncated.

Operators can pair an internal generation adapter at startup through
`tenancy.private_adapter`. Use exact HTTPS generation URLs at one origin and
a dedicated 32-byte key from the named environment variable. A paired
endpoint requires a profiled ordered generation request and a peer that
authenticates and consumes each attempt before sending it upstream. See the
[configuration contract](../../../docs/specs/configuration/README.md#private-generation-adapter).

Ordered selections require an explicit `admission_mode` (`legacy` or
`profiled_v1`), matching the retained root; missing mode is not a default.
Installation authority alone can provision admission profiles or profiled
routes through the existing resource API. Profiles use explicit rate/health
rules; zero RPM is valid local refusal. A profiled route permanently protects
its key, including old revisions and configurations. Do not downgrade a denied
profiled request to legacy or v1. V1 profiles keep local health. For explicit
shared health across distinct members, verify enabled V2 resource discovery and
use format-two profile puts with `health_group_id`; never downgrade unsupported
V2 writes. Group rules and membership remain immutable, including revoked history.
New authority stores use format four and refuse unreleased format-two stores;
the supported upgrade preserves the historical 1-to-3 step before 3-to-4.
Retain a compatible stopped-writer backup
before changing binaries. See the [service contract](../../../docs/specs/service-api/README.md#resource-provisioning)
for exact profile, import-head and protection semantics.
Use `observe_rate_group_id` only for an explicit same-owner read-only association.
It can expose a measured shared count while `rate:null` remains uncharged;
absence is unavailable, never an assumed zero. Read through the existing operator
candidate-manifest endpoint, retain the instance identity, and do not treat the
sample as admission authority or a durable/fleet-wide counter.
The optional execution total starts at admission; `null` or omission permits
ongoing active work. The idle setting bounds each provider read and blocked
output handoff, preventing silent upstreams or unread response bodies from
indefinitely retaining reservations. These lifetime durations are positive integer seconds
up to 31,536,000. Preparation callbacks follow the execution's optional total;
per-part callbacks use the idle budget and storage keeps its shorter limits.
A disconnected preparation waiter cancels its owned call. Receipt settlement
has a fixed 30-second budget; cancellation/shutdown has a separate 30-second
producer-join budget so unread output does not spend cleanup time in advance.
Repeated cancellation does not renew either budget. Late confirmed joins can
release capacity, but genuine cleanup failure keeps admission fenced. Unknown
effects remain unknown and inference is never replayed. Successful delivery
still waits for durable receipt acknowledgement.

`resource_origins` defaults to empty, which keeps tenant-managed route
provisioning disabled. Installation operators remain trusted. An enabled host
may list up to 256 distinct HTTP(S) origins (for example,
`https://api.example.com`); a trailing slash is accepted. Userinfo, paths,
queries, fragments and duplicate normalized origins are rejected. This is a
host-controlled allowlist, not a field in a tenant draft.

`tenancy.resource_import` optionally imports an operator JSON resource manifest
at startup; relative paths use the server process working directory. Set
`tenancy.wrapping_key_env` to an environment variable holding a stable base64
32-byte key. Import requires that key, and retained resources require the same
key on later restarts even after removing the import path. These settings are
restart-only. Never treat a replacement key as automatic rotation. The importer
accepts explicit route/policy definitions, static API-key credential slots, and
explicit Claude Code OAuth seeds pinned to the known Anthropic registration
under the [owned OAuth profile](../../../docs/development/tenancy-auxiliary.md).
Ambient credential backends and native request checks remain unsupported. Definitions
are retained as authenticated ciphertext, and a changed definition requires a
new qualified resource revision. Importing resources does not grant tenant access
or activate a configuration. Never put resource-import definitions or secret key
values into a tenant draft; drafts contain resource references only.

Use the HTTP `TenancyClient` for tenant management or the `bro tenancy`
configuration adapter below. The tenant-operation namespace
is `/control/v1/tenancy/operations/{operation_id}`, separate from host reload
operations. Retain operation IDs after a lost response; credential secrets are
returned only once. Tenancy is disabled by default. In the supported enabled
profile, `dispatch_enabled`, `revision_bound_dispatch` and `strict_receipts`
reflect the owning service's current eligibility. Shutdown, saturation or a
storage/admission fence prevents new dispatch; unavailable authority can also
prevent discovery. Require all three flags before strict inference. True flags
neither reserve execution capacity nor grant access to a route or credential.

Use `TenantCapabilities::bind` with an explicitly selected retained execution
revision and correlation ID, and send its seven binding headers with the issued
tenant credential. An installation operator token does not authorize inference.
The service rechecks the exact installation, instance and capability revision,
current grants, retained resources and request limits before starting work.
After a restart, discover the new instance descriptor for a new submission;
do not reuse the old binding. If a response is uncertain, retain any returned
execution ID and the original correlation ID, then query receipts with current
read authority. Correlation is not an idempotency key: do not automatically
refresh the binding and resend the inference request.

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

For ordered per-call generation defaults, freeze the binding returned by
`OrderedDispatchBindingV2::with_generation_defaults`, then attach the same
closed object with `OrderedTenantGateway::with_generation_defaults`. Use only
the nine canonical fields in the service API specification. Keep the object in
the existing bounded body, never in provider extras or a custom header. Typed
null and empty stop permit default filling; explicit typed values win. An
unused extension leaves existing request and selection bytes unchanged.

For opaque provider defaults, use `GenerationDefaultsEnvelopeV1`, bind it with
`with_generation_defaults_envelope`, and attach the same object on the ordered
gateway. Put the nine canonical fields in `canonical` and other parameter
defaults in `supplemental`. The two defaults body families are mutually
exclusive. Supplemental JSON cannot change selected routing, authentication,
HTTP headers or continuation permissions. See the service API contract for
caller precedence, null/invalid fallthrough, hash domains and exact reserved
names; never copy supplemental fields into ingress DTO extras.
