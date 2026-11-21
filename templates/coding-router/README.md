# Minimal `bitrouter/coding` router

Copy `bitrouter.yaml`, configure credentials for your providers, and replace
both model placeholders with routable `provider:model` selectors from `bitrouter models`:

```bash
bitrouter policy init coding \
  --strong provider:strong-model \
  --economy provider:economy-model \
  --config bitrouter.yaml
bitrouter policy check --config bitrouter.yaml
bitrouter policy show coding --disk --config bitrouter.yaml
bitrouter serve --config bitrouter.yaml
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

Initialization validates the selectors and confirms that the binding is
compatible. Its report marks configuration as `saved_only`; explicitly reload or
start the daemon before requesting `bitrouter/coding`.

`policy show` defaults to active state. `--disk` reads the saved source and
returns a complete source document. Updates use a separate candidate file and
`bitrouter policy publish candidate.yaml --expected-digest <source-digest>`.
