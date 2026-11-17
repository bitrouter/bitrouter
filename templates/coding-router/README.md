# Minimal `bitrouter/coding` router

Copy `bitrouter.yaml`, configure credentials for your providers, and replace
both model placeholders with routable `provider:model` selectors from `bro models`:

```bash
bro policy init coding \
  --strong provider:strong-model \
  --economy provider:economy-model \
  --config bitrouter.yaml
bro policy check --config bitrouter.yaml
bro policy show coding --disk --config bitrouter.yaml
bro serve --config bitrouter.yaml
```

Omitting `--router` selects `coding`. Initialization creates model-only
`policy.yaml` and saves this explicit router binding:

```yaml
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: provider:strong-model
```

Neither model is a release default. Initialization validates the selectors,
refuses incompatible bindings, and preserves existing chat and harness
settings. Its report marks configuration as `saved_only`; explicitly reload or
start the daemon before requesting `bitrouter/coding`.

`policy show` defaults to active state. `--disk` reads the saved source and
returns a complete source document. Updates use a separate candidate file and
`bro policy publish candidate.yaml --expected-digest <source-digest>`.
