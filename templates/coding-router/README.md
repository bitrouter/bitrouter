# Minimal `bitrouter/coding` router

Copy `bitrouter.yaml`, configure credentials for the providers you intend to
use, and replace both model placeholders with routable `provider:model`
selectors from `bro models`:

```bash
bro policy init coding \
  --strong provider:strong-model \
  --economy provider:economy-model \
  --config bitrouter.yaml
```

Omitting `--router` targets `coding`. The command writes the ordinary policy
lock plus this explicit router binding:

```yaml
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: provider:strong-model
```

Neither model is a release default. Initialization validates both selectors and
refuses missing or incompatible existing bindings. It preserves the template's
`policy.mode: frozen` and any existing `chat` or harness settings. After it
succeeds, use `bitrouter/coding` as the request's model selector.
