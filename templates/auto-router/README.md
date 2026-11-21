# Explicit `bitrouter/auto` source

This example configures a named `auto` router with an editable `policy.yaml`.
The source has three model tiers, a strong default and tool-safe fallback rules. Every request retains its full admitted context. A target's
optional `effort` replaces caller effort; omitting it preserves caller effort.

Copy both YAML files to a directory, configure the providers' credentials,
then validate and start from that directory:

```bash
bitrouter policy check --config bitrouter.yaml
bitrouter policy show auto --disk --config bitrouter.yaml
bitrouter serve --config bitrouter.yaml
```

Request `bitrouter/auto`. `policy show` defaults to the selected running
instance; `--disk` explicitly reads the saved source. Its `source` field is a
complete `policy::Document` that can also be submitted to the Policy service.
Neither initialization nor evaluation changes an active policy automatically.

To update, write a separate current-format source, inspect its changes, and use
the exact digest from the disk report as the compare-and-swap argument:

```bash
bitrouter policy diff policy.yaml candidate.yaml
bitrouter policy publish candidate.yaml --expected-digest <source-digest> --config bitrouter.yaml
bitrouter policy rollback <previous-source-digest> --expected-digest <current-source-digest> --config bitrouter.yaml
```

Publication and rollback reload a reachable local daemon. A rejected reload
restores the previous source with CAS and reloads it; a concurrent winner is
never overwritten. `bitrouter policy reload --config bitrouter.yaml` explicitly
reloads saved configuration and source on the selected daemon.

The source uses `spec_version: policy/1.0.0-beta.1` with model and effort
targets. To add classified routes, supply matching classifier configuration
and evaluate the exact revision before publishing.
