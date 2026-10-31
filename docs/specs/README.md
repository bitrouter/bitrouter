# BitRouter protocol specifications

This directory contains version-controlled protocol contracts for independent
producers and consumers. It is the authoritative home for wire formats, data
schemas, interoperability rules, compatibility guarantees, and observable
conformance requirements. The index below distinguishes beta drafts from their wire version.

## Protocol index

| Protocol | Status | Specification | Conformance |
| --- | --- | --- | --- |
| Registry v1 | Beta, `1.0.0-beta.6`; wire version `1` | [Registry v1](registry-v1/README.md) | [Cases](registry-v1/conformance.md) |
| Control HTTP | Beta, `1.0.0-beta.1`; `/control/v1` | [Control](control-v1/README.md) | [Cases](control-v1/conformance.md) |
| Tenancy HTTP | Beta, `1.0.0-beta.1`; `/control/v1` | [Tenancy](tenancy-v1/README.md) | [Cases](tenancy-v1/conformance.md) |
| Inference gateway | Beta, `1.0.0-beta.1`; vendor protocol versions | [Gateway](gateway-v1/README.md) | [Cases](gateway-v1/conformance.md) |
| MCP gateway | Beta, `1.0.0-beta.1`; negotiated MCP version | [MCP gateway](mcp-gateway-v1/README.md) | [Cases](mcp-gateway-v1/conformance.md) |
| Shared operator | Beta, `1.0.0-beta.1`; `/operator/v1` | [Operator](shared-operator-v1/README.md) | [Cases](shared-operator-v1/conformance.md) |
| Eval exchange | Beta, `1.0.0-beta.1`; `/v1/evals` | [Eval](eval-v1/README.md) | [Cases](eval-v1/conformance.md) |
| Native Thread HTTP | Beta, `1.0.0-beta.1`; `/agent/v2` | [Thread API](agent-http-v2/README.md) | [Cases](agent-http-v2/conformance.md) |
| Managed Core | Beta, `1.0.0-beta.1`; core wire `1` | [Managed Core](managed-core-v1/README.md) | [Cases](managed-core-v1/conformance.md) |
| Native ACP | Beta, `1.0.0-beta.1`; ACP v1 / draft v2 | [Native ACP](native-acp-v1/README.md) | [Cases](native-acp-v1/conformance.md) |
| Agent catalog | Beta, `1.0.0-beta.1`; no envelope version | [Agent catalog](agent-catalog-v1/README.md) | [Cases](agent-catalog-v1/conformance.md) |
| Policy lock | Beta, `1.0.0-beta.1`; file version `4` | [Policy lock v4](policy-lock-v4/README.md) | [Cases](policy-lock-v4/conformance.md) |

Beta describes the maturity of the contract. It does not assert that a particular
release implements it. Stable protocols must keep their compatibility promises;
an incompatible beta revision must be identified explicitly.

## What belongs here

- Message, API, file, and artifact formats that independent implementations must
  agree on, including identity, units, ordering, and default values.
- Producer and consumer obligations, error behavior, authentication boundaries,
  synchronization semantics, and compatibility with unfamiliar data.
- Versioning and extension rules, schemas, synthetic examples, and behavioral
  conformance cases for the protocol.

Implementation plans, module ownership, refactor designs, task lists, progress
reports, dated acceptance results, and temporary investigation notes belong in
development work documents or pull requests. Local working plans may use the
gitignored `docs/superpowers/` directory. Do not add `*_IMPLEMENTATION.md` or
feature-delivery `*_SPEC.md` documents here. A filename containing "spec" does
not make a development document a protocol standard.

Older development documents elsewhere in `docs/` are not protocol specifications
by implication. Promotion requires a reviewed contract that removes transient
implementation details and establishes explicit compatibility rules.

## Layout and authorship

1. Give each protocol major a directory such as `registry-v1/`. Its `README.md`
   is the single normative entry point; keep related schemas, examples, and
   `conformance.md` inside that directory. Add it to the index above.
2. State the protocol's scope, status, wire version, and specification revision.
   Define normative terms such as MUST, MUST NOT, SHOULD, and MAY. Clearly label
   informative examples and external references.
3. Specify externally observable behavior. Avoid making crate paths, branches,
   pull request bases, implementation stages, or current test results part of
   the contract. Cross-implementation conformance requirements remain here;
   evidence that a particular implementation passed belongs with that change.
4. Define how old and new peers handle unknown fields and values, unsupported
   required features, malformed data, and recoverable errors. Document the
   compatibility consequences of every revision before changing a stable
   format, identifier, unit, default, or required behavior.
5. Update the contract, schemas when present, examples, and conformance cases
   together. Check links, parse examples, and verify their semantic consistency.
   Examples must not depend on live commercial model membership or credentials.

These specifications and their supporting files must be committed to Git and
reviewed with protocol changes. Keep `docs/specs/` out of working-document ignore
rules. Superseded stable contracts must remain accessible to their consumers;
remove temporary implementation material rather than accumulating it here.
