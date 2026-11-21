# Model-only routing

`bitrouter/auto` addresses a configured router named `auto`. Other named
routers use `bitrouter/<id>`. Select a configured name before sending traffic.

Start with a saved `bitrouter.yaml`. Use `bitrouter init` to create one if
needed, then choose model selectors from `bitrouter models`.

```bash
bitrouter policy init auto --router auto \
  --strong provider:strong-model --economy provider:economy-model
bitrouter policy check --config bitrouter.yaml
bitrouter policy show auto --disk --config bitrouter.yaml
bitrouter serve --config bitrouter.yaml
```

Initialization saves `policy.yaml` and a router binding and reports
`config_activation: saved_only`.
Explicitly reload or start the daemon before using that saved configuration.
A missing configured source fails validation.

## Source and execution

The portable document has `spec_version: policy/1.0.0-beta.1` and named
`policies`. Every tier is `{model, effort?}`. A target effort replaces caller
effort. Omission preserves caller effort. Requests retain all admitted
context. Tool-bearing requests use the source's explicit tool-safe tier rules.

Classified routes use typed `RouteClass` keys in `task-family|next-step|risk`
form and require the exact `classifier_digest` for the configured classifier. Missing or low-confidence
classifications abstain to the default. Configure `decision_model.model` and
its credential environment variable to use the TypeSafe protocol adapter;
other backends require a compatible adapter. A URL alone does not translate
protocols. Changing backend configuration requires restart.

Router bindings select literal policy names. Different names in one source
are independent policies.

## Inspect and update

`policy status` and `policy show [name]` default to the selected instance's
active state. Use `--disk` or `--view disk` for saved source, including offline
inspection. An unavailable instance never silently becomes a disk read.
`show` returns the complete resubmittable document in `source`, with view,
availability, source digest and router bindings in the surrounding report.

```bash
bitrouter policy diff policy.yaml candidate.yaml
bitrouter policy publish candidate.yaml --expected-digest <current-source-digest> --config bitrouter.yaml
bitrouter policy rollback <previous-source-digest> --expected-digest <current-source-digest> --config bitrouter.yaml
bitrouter policy reload --config bitrouter.yaml
```

CAS is an explicit argument, never inferred from document contents. Updates
atomically replace source and reload a reachable daemon. A rejected reload
restores the prior current-format source with CAS and reloads it. Concurrent
changes are preserved and reported. Local updates use file/process authority;
service activation separately requires authentication, scopes, and CAS.

[Evaluation and explicit updates](workflow-optimization.md) describes the
client-driven evaluation workflow. Evaluation facts and drafts never activate
source changes automatically. ACL files are separate and use
`bitrouter access-policy create`, then `bitrouter key sign --policy`.
