---
type: added
title: "`bitrouter/auto` is the published slug for policy-driven routing"
pr: 788
---

Send `bitrouter/auto` as the model to route through your policy table.
`vendor/auto` is the convention the gateway ecosystem already uses, so pointing
an existing config at BitRouter changes one segment of a model id rather than
teaching a new addressing syntax.

The whole `bitrouter/` prefix is reserved. Resolution happens in
`resolve_presets`, the choke point every consumer passes through, so the CLI
preview and the daemon cannot disagree; and
`dist-helper registry validate` now rejects any catalog **or
provider-declared** model id under that prefix — relevant if you contribute
provider YAML, where such ids were previously only an advisory.

`@auto` keeps working. It was never an auto-specific name, just the generic
`@preset[:variant]` form every preset uses, so existing configs are unaffected —
the spelling is simply no longer advertised.

The slug needs setup: it reports
`'bitrouter/auto' needs a preset named 'auto' bound to a routing policy; run
'bitrouter optimize setup'` when nothing is bound, and it does **not** work on a
fresh install, because `zero_config()` defines no presets. `bitrouter:auto` (the
colon form) is rejected with a pointer to the slash form, and an unknown slug
under the namespace is a `400`, not a degraded provider `404`.
