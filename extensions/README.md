# Extensions

This directory groups independently delivered capabilities by ownership. Each
extension keeps its related Cargo packages, developer entry point and usage
documentation together. Shared host contracts stay in `crates/`; product hosts
stay in `apps/`.

| Extension | Packages | Delivery |
| --- | --- | --- |
| [Guardrails](guardrails/README.md) | `bitrouter-guardrails`, `bitrouter-guardrails-service` | Reusable matcher with optional SDK hooks; independent HTTP input checker |

An extension may be linked explicitly into a trusted custom host or run as a
separate service implementing a supported capability contract. Directory
placement does not provide runtime isolation, automatic installation or dynamic
loading. Each extension documents its actual supported integration paths.

The workspace includes packages at `extensions/<extension>/<package>/`. Keep
Cargo package names stable when moving source, and verify dependency isolation
and release configuration separately from directory placement.
