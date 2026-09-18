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

The workspace includes packages at `extensions/<extension>/<package>/`. Keep
Cargo package names stable when moving source, and verify dependency isolation
and release configuration separately from directory placement.
