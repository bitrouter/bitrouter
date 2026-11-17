# Explicit `bitrouter/auto` source

The zero-config router uses the model-only policy bundled with the binary.
This directory supplies the same source as an explicit, editable `policy.yaml`.
It has three model tiers, a strong default, tool-safe fallback rules, and no
learned routes. Every request retains its full admitted context. A target's
optional `effort` replaces caller effort; omitting it preserves caller effort.

Copy both YAML files to a directory, configure the providers' credentials,
then validate and start from that directory:

```bash
bro policy check --config bitrouter.yaml
bro policy show auto --disk --config bitrouter.yaml
bro serve --config bitrouter.yaml
```

Request `bitrouter/auto`. `policy show` defaults to the selected running
instance; `--disk` explicitly reads the saved source. Its `source` field is a
complete `policy::Document` that can also be submitted to the Policy service.
Neither initialization nor evaluation changes an active policy automatically.

To update, write a separate current-format source, inspect its changes, and use
the exact digest from the disk report as the compare-and-swap argument:

```bash
bro policy diff policy.yaml candidate.yaml
bro policy publish candidate.yaml --expected-digest <source-digest> --config bitrouter.yaml
bro policy rollback <previous-source-digest> --expected-digest <current-source-digest> --config bitrouter.yaml
```

Publication and rollback reload a reachable local daemon. A rejected reload
restores the previous source with CAS and reloads it; a concurrent winner is
never overwritten. `bro policy reload --config bitrouter.yaml` explicitly
reloads saved configuration and source on the selected daemon.

The source accepts only `spec_version: policy/1.0.0-beta.1` and model/effort
selection fields. Explicit missing files, old lock documents, context actions,
guards, presets, variants, and runtime modes are rejected. To add classified
routes, supply matching classifier configuration and evaluate the exact revision
externally before explicitly publishing a new source.
