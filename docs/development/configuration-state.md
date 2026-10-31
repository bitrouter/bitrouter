# Configuration-state comparison

The daemon compares its own saved source with the state it actually loaded.
Clients, including remote CLI consumers, use that target's evidence rather than
combining local disk data with another process's running state. A successful file
write does not mean the process applied it.

Saved source state distinguishes available, generated, missing, invalid and
unavailable. Running state distinguishes in-sync, reload-required,
restart-required, mixed and unknown. Separate policy/access-policy sources need
their own evidence. A missing optional field on an older peer is unknown, not
proof of equality. Comparisons stay inside the process: reports contain safe
field categories, instance/generation and reload results, without secret values
or value-derived hashes.

Startup-owned settings are rejected consistently by reload before any
participant changes. Reloadable participants can still fail after an earlier
participant applied a change, so mixed-state history is retained. A later
preparation failure cannot erase evidence of an earlier partial application.
Disk corruption or a saved listener-path edit does not establish that an already
running daemon stopped; discovery must retain the actual owner identity.

Configuration format and migration compatibility are maintained in the
[configuration spec](../specs/configuration/README.md). Report fields and
reload operations are maintained in the
[service API](../specs/service-api/README.md#control-api). Implementation lives in
[reload](../../apps/bitrouter/src/reload.rs) and the application configuration
and discovery modules. Update the shared reports and all local/remote consumers
together when the comparison behavior changes.
