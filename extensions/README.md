# Extensions

This directory groups extension implementations by ownership. A capability is
a host contract (such as request-check); an extension supplies an implementation. Each
extension keeps its related Cargo packages, developer entry point and usage
documentation together. Shared host contracts stay in `crates/`; product hosts
stay in `apps/`.

| Extension | Packages | Delivery |
| --- | --- | --- |
| [Regex checker](regex-checker/README.md) | `bitrouter-guardrails`, `bitrouter-regex-checker` | Explicit native callback or independent HTTP service; legacy SDK hooks remain optional |

An extension may be linked explicitly into a trusted custom host or run as a
separate service implementing a supported capability contract. Directory
placement does not provide runtime isolation, automatic installation or dynamic
loading. Each extension documents its actual supported integration paths.

## Author entry point

For a compiled request-check extension, write an ordinary registration function
that accepts `bitrouter::extension::ExtensionApi` and calls
`request_check(id, revision, callback)`. A custom host invokes it with
`assemble::build_app_with_extensions`. The API collects implementations; router
`checks.request` bindings determine when they execute. Duplicate instance IDs,
invalid revisions, registration failures and activation mismatches prevent the
host from becoming ready. See the [regex example](regex-checker/README.md).

This is one typed author entry point, currently exposing only request-check.
It is not a generic event handler, dynamic loader or permission sandbox, and
it does not expose the host builder, global hooks, credentials or migrations.
HTTP implementations use the existing versioned request-check protocol and do
not need to call Rust registration code.

`Plugin`, `AppBuilder::plugin`, and the matcher's optional `GuardrailsPlugin`
remain legacy custom-host assembly APIs. They retain their global/output and
migration semantics during the current alpha SDK compatibility window. Removal
requires an explicitly announced breaking SDK release with migration notes;
there is no scheduled removal date. They are not an alternative recommended
entry point for new request-check extensions. `PluginId` metadata ownership,
`Config::plugins`, typed Context `extensions`, and external agent-plugin manifest
formats keep their existing purposes and names.

The workspace includes packages at `extensions/<extension>/<package>/`. Keep
Cargo package names stable when moving source, and verify dependency isolation
and release configuration separately from directory placement.
