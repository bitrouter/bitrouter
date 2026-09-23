# Extensions

This directory groups extension implementations by ownership. A capability is
a host contract (such as request-check); an extension supplies an implementation. Each
extension keeps its related Cargo packages, developer entry point and usage
documentation together. Shared host contracts stay in `crates/`; product hosts
stay in `apps/`.

| Extension | Packages | Delivery |
| --- | --- | --- |
| [Regex checker](regex-checker/README.md) | `bitrouter-guardrails` | Compiled request-check callback; legacy SDK hooks remain optional |
| [System One JSON](system-one/README.md) | `bitrouter-system-one-format` | Compiled evaluation format; TypeSafe is the first provider binding |

Beta extensions are linked explicitly into a trusted custom Rust host. Adding
or updating extension code requires rebuilding that host. There is no remote
extension protocol, independent extension process manager or dynamic loader.
Directory placement does not provide runtime isolation or automatic installation.

## Author entry point

For a compiled request-check extension, write an ordinary registration function
that accepts `bitrouter_sdk::extension::ExtensionApi` and calls
`request_check(id, revision, callback)`. A custom host invokes it with
`host::serve_with_extensions` to run the shared foreground daemon, or
`assemble::build_app_with_extensions` for low-level embedding. The API collects implementations; router
`checks.request` bindings determine when they execute. Duplicate instance IDs,
invalid revisions, registration failures and activation mismatches prevent the
host from becoming ready. See the [regex example](regex-checker/README.md).

Valid registrations absent from `checkers` stay inactive and produce a sorted
startup diagnostic. They have no runtime entry or management inventory. Configured
instances require matching registrations even when no router binds them.

For an evaluation-format extension, register an
`extension::evaluation_format::EvaluationFormatAdapter` with
`ExtensionApi::register_evaluation_format`. An active provider's
`operations.evaluate.format` binds the exact extension id, adapter id, and
revision. Missing or mismatched bindings fail before database assembly;
unbound formats remain inactive. The host owns the provider endpoint,
credentials, HTTP transport, retries, and settlement. See
[System One JSON](system-one/README.md).

This is one typed author entry point with request-check and evaluation-format
capabilities.
It is not a generic event handler, dynamic loader or permission sandbox, and
it does not expose the host builder, global hooks, credentials or migrations.
Capability inputs and decisions live alongside this entry point in
`bitrouter_sdk::extension::request_check`; they contain no HTTP envelope.
Extensions may call external services internally, without a host-managed remote
extension transport.

`Plugin`, `AppBuilder::plugin`, and the matcher's optional `GuardrailsPlugin`
remain legacy custom-host assembly APIs. They retain their global/output and
migration semantics during the current alpha SDK compatibility window. Removal
requires an explicitly announced breaking SDK release with migration notes;
there is no scheduled removal date. They are not an alternative recommended
entry point for new request-check extensions. `PluginId` metadata ownership,
`Config::plugins`, typed Context `extensions`, and external agent-plugin manifest
formats keep their existing purposes and names.

The workspace includes packages at `extensions/<extension>/<package>/`. Keep
published Cargo package names stable when moving source; an unreleased format
package can be renamed before its first integration. Verify dependency
isolation and release configuration separately from directory placement.
