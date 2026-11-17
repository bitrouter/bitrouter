# Model-only routing

`bitrouter/auto` uses the bundled model-only source in a zero-config local
installation. It has three model tiers, a strong default, tool-safe fallback,
and no learned routes. Explicit named routers use `bitrouter/<id>`.
The reserved namespace is resolved locally; unknown names fail.

Use `templates/auto-router` for explicit source or initialize a custom router:

```bash
bro policy init coding --router coding --strong provider:strong-model --economy provider:economy-model
bro policy check --config bitrouter.yaml
bro policy show coding --disk --config bitrouter.yaml
bro serve --config bitrouter.yaml
```

Initialization saves `policy.yaml` and a router binding. It preserves unrelated
chat and harness settings and reports `config_activation: saved_only`.
Explicitly reload or start the daemon before using that saved configuration.
A configured missing source fails; the bundled source never replaces it.

## Source and execution

The portable document has `spec_version: policy/1.0.0-beta.1` and named
`policies`. Every tier is `{model, effort?}`. A target effort replaces caller
effort; omission preserves it. Requests retain all admitted context. Policy
selection does not run tools, rewrite context, explore, or publish learned
routes. Tool-bearing requests use the source's explicit tool-safe tier rules.

Classified routes use `task-family|next-step|risk` keys and require the exact
`classifier_digest` for the configured classifier. Missing or low-confidence
classifications abstain to the default. Configure `decision_model.model` and
its credential environment variable to use the TypeSafe protocol adapter;
other backends require a compatible adapter. A URL alone does not translate
protocols. Changing backend configuration requires restart.

Router bindings select literal policy names. Different names in one source
are independent policies, with no request variant fallback. Old lock files,
context targets, quality certificates, runtime modes, and preset configuration
are rejected. Write a fresh current source for a new installation.

## Inspect and update

`policy status` and `policy show [name]` default to the selected instance's
active state. Use `--disk` or `--view disk` for saved source, including offline
inspection. An unavailable instance never silently becomes a disk read.
`show` returns the complete resubmittable document in `source`, with view,
availability, source digest and router bindings in the surrounding report.

```bash
bro policy diff policy.yaml candidate.yaml
bro policy publish candidate.yaml --expected-digest <current-source-digest> --config bitrouter.yaml
bro policy rollback <previous-source-digest> --expected-digest <current-source-digest> --config bitrouter.yaml
bro policy reload --config bitrouter.yaml
```

CAS is an explicit argument, never inferred from document contents. Updates
atomically replace source and reload a reachable daemon. A rejected reload
restores the prior current-format source with CAS and reloads it. Concurrent
changes are preserved and reported. Local updates use file/process authority;
service activation separately requires authentication, scopes, and CAS.

[Evaluation and explicit updates](workflow-optimization.md) describes the
client-driven evaluation workflow. Evaluation facts and drafts never activate
source changes automatically. ACL files are separate and use
`bro access-policy create`, then `bro key sign --policy`.
