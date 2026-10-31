# Development guide

Read the [project introduction](../README.md), then the
[workspace architecture](development/architecture.md) and
[development documentation index](development/README.md) for the current source
layout, implementation ownership and operational boundaries.

## Documentation policy

Keep three kinds of maintained material distinct:

- [`specs/`](specs/README.md): carefully designed and reviewed contracts for
  serialized configurations, artifacts and service messages whose changes can
  break independent readers, clients or retained data. A topic qualifies only
  when it has real consumers and compatibility consequences worth maintaining.
- [`development/`](development/README.md): implementation, architecture, module
  ownership and behavioral boundaries. These guides evolve with the application;
  they do not create a separately versioned wire or file format.
- UX and operator instructions: preserve useful interactions and procedures.
  [`ux/`](ux/README.md) identifies historical designs; the [CLI reference](CLI.md)
  and deployment guides describe current operations. Product-site documentation
  is maintained in [bitrouter-docs](https://github.com/bitrouter/bitrouter-docs).

### Specification admission and review

Do not create a spec for each feature, transport, adapter or architectural rule.
One object has one canonical specification: combine related messages, profiles,
compatibility rules and cases there. Schemas and synthetic fixtures may be
supporting files; they are not competing specifications. Use an existing
machine-readable schema as the field catalog rather than duplicating it in
another prose spec. Internal agent catalogs, gateway/MCP adaptation and exporter
ownership are engineering topics unless a distinct external format needs a
reviewed compatibility commitment.

Before adding or substantially changing a spec, explain its consumers and why
it requires a maintained compatibility contract in the PR. Review old-reader /
new-writer and new-reader / old-writer behavior, including defaults, units,
nullability, identity, ordering, errors, extensions, data migration and rollback.
Keep format revision and beta/stability status distinct from software release
numbers. Review the specification, schemas/DTOs, examples and meaningful
compatibility cases together. Beta does not waive design or review. Document
unimplemented proposals as proposals, not current supported formats. Do not
remove a published stable contract still needed by its consumers.

### Cross-reference review

Review why each reference is needed, as well as whether its path and anchor
resolve. If another document defines a necessary format or engineering rule,
keep that document in the appropriate maintained directory, or consolidate the
rule and update all consumers. A guide must not depend on a deleted document or
on an obsolete historical version for its current behavior.

Remove references to unnecessary plans, progress, acceptance logs and experiment
outputs, including the surrounding statements that only direct readers there.
Do not turn a deleted-document link into a Git-history permalink merely to make
a link check pass. Do not erase a broken link's markup while leaving the same
unsupported reference in prose. Historical document links are appropriate only when a
specific retained version is itself necessary to understand compatibility or an
explicit migration. Source citations to upstream standards remain useful.

Before moving or removing a document, inspect Markdown links, inline filenames,
source comments, generated descriptions, skills and build/test consumers. Update
paths and anchors together. Keep required test fixtures in their contract/test
location and update consumers before moving them. Audit both directions: all
references resolve to necessary maintained content, and required rules remain
available without following a chain of removed documents.

### Working material

Task plans, implementation trackers, agent handoffs, dated test logs, raw
experiment outputs and acceptance receipts belong in their PR, issue, CI
artifact or untracked local work area. Do not add them to tracked `docs/`.
Current engineering rules belong in development guides and should be updated
when the relevant code changes. Development and audit agents must apply this
policy when writing or reviewing repository documentation.
