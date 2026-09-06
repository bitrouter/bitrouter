---
type: changed
breaking: true
title: "`bitrouter route --json` and MCP `route_preview` share one report shape"
pr: 869
---

`bitrouter route` and the MCP `route_preview` tool are now one action over one
report type, and the report keeps `route_preview`'s richer vocabulary — it was
the superset, and it is the one an agent reads. Duplicated keys for one fact are
exactly the drift the unification exists to remove, so the old names are gone
rather than deprecated.

```jsonc
// before — `bitrouter route gpt-5 --json`
{
  "model": "gpt-5",
  "resolved_via": "config",
  "chain": [{ "provider": "openai", "service_id": "gpt-5", "protocol": "openai" }]
}

// after
{
  "requested_model": "gpt-5",          // was `model`
  "effective_model": "gpt-5-codex",    // new: what the policy table selects
  "effective_effort": "high",          // new, omitted when policy selects none
  "resolved_via": "config",            // now live | config | zero_config
                                       // (was live daemon | config | zero-config)
  "policy_decision": { … },            // new, omitted on the live-daemon path
  "provider_chain": [                  // was `chain`
    { "provider": "openai", "service_id": "gpt-5-codex", "api_protocol": "openai" }
                                       // `protocol` → `api_protocol`
  ],
  "estimated_cost": { … }              // new, omitted when the registry prices nothing
}
```

Migration is mechanical: `.model` → `.requested_model` (read `.effective_model`
if you want what would actually run), `.chain` → `.provider_chain`,
`.chain[].protocol` → `.provider_chain[].api_protocol`, and
`.resolved_via == "live daemon"` → `"live"` / `"zero-config"` → `"zero_config"`.
The `resolved_via` values are now the same words `bitrouter models --json` uses
for the same fact (`live` / `config`), instead of two spellings that almost
matched.

Two behaviour fixes ride along. `bitrouter route` now runs the **policy table**
in its config fallback, as `route_preview` always did — it could previously name
a model the daemon would never pick, which is why `effective_model` is a
separate field from `requested_model`. (The live-daemon path is unchanged on
both surfaces: the daemon's `route` verb resolves the model as given, so a
`live` answer reports `effective_model == requested_model` and no
`policy_decision`.) And `route_preview` now resolves config **per call** instead
of snapshotting it at `bitrouter mcp serve` start, so an edited `bitrouter.yaml`
is visible to a long-lived MCP server, as it always was to the CLI.
