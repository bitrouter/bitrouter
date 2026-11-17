# Evaluate and explicitly update model policy

Run evaluation in a client or external evaluator. The service accepts facts
about exact revisions; it does not launch evaluation jobs or automatically
learn and publish routing changes.

## Bind evidence to the evaluated revision

Tenant model-policy subjects use schema 2 and the `tenant_model_policy`
identity profile. Bind the service installation, tenant, configuration revision,
full immutable Policy `ResourceRef`, and literal selected policy name. Source
and classifier digests are supporting evidence, never substitutes for resource
identity or authorization. Execute inference against that exact configuration.

Policy-free tenant subjects use `tenant_generic`; gateway evaluation uses
`gateway_generic`. Submit each profile through its corresponding exchange.
Generic facts cannot qualify another Policy resource or revision for an update.
Results inherit the validated subject's identity; result schema remains 1.

The local gateway exchange supports:

```bash
bro eval subject seal subject-draft.json --output subject.json
bro eval subject put subject.json --config bitrouter.yaml
bro eval result submit result.json --config bitrouter.yaml
bro eval snapshot freeze --config bitrouter.yaml
```

Use the tenant HTTP/SDK exchange for tenant resources and exact revision-bound
inference. A local gateway ledger never supplies tenant authority. Preserve
router-authored decision identity verbatim and do not invent experiment or
resource references. Aggregate only facts with the same complete identity.
Unknown usage stays unknown; do not manufacture zero cost or task success from
a successful request.

## Author and publish a new source

Review frozen facts, then explicitly author a new model-only `policy::Document`.
The source contains model/effort tiers and selection rules. Evidence, context
transformations, certificates, and optimization state do not belong in it.

For a local file-backed router:

```bash
bro policy show --disk --config bitrouter.yaml
bro policy diff policy.yaml candidate.yaml
bro policy publish candidate.yaml --expected-digest <current-source-digest> --config bitrouter.yaml
```

`source` from `show` is directly resubmittable. Pass the reported
`source_digest` separately as `--expected-digest`. A stale digest refuses the
write. Publication reloads a reachable daemon and restores the prior source
on reload rejection without overwriting a concurrent publisher.

For tenant management, create an immutable Policy revision and independently
authorize/CAS activation through its configuration. Subject submission, result
submission, snapshot freezing, and draft creation never change active routing.
To restore a local current-format source, use `policy rollback <digest>
--expected-digest <current-source-digest>`.

Experimental BRO algorithms have independent ownership and objects. They do
not exchange retired lock documents with mature model-policy services.
