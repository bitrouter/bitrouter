# BitRouter engineering docs

This directory contains internal engineering knowledge that is useful beyond a
single implementation workflow. It is not the BitRouter product documentation.
User-facing documentation is authored and published from
[`bitrouter-docs`](https://github.com/bitrouter/bitrouter-docs).

## What belongs here

Add a document only when the information cannot be expressed more reliably in
code, types, schemas, tests, or generated output, and at least one of these is
true:

- multiple components or development workflows need the same system model;
- a security, protocol, or compatibility invariant is difficult to infer from
  the implementation;
- the rationale for a durable decision will matter when it is challenged; or
- active multi-phase work needs a checked-in specification or execution state.

Do not add command tutorials, exhaustive CLI reference, user onboarding,
copied upstream documentation, completed execution journals, or a second copy
of facts already owned by a skill. Git history, issues, and pull requests are
the archive for completed or superseded work.

## Structure

- [`architecture/`](architecture/) — current cross-cutting system models.
- [`invariants/`](invariants/) — safety and compatibility constraints whose
  violation may still compile.
- [`decisions/`](decisions/) — durable decisions and rejected alternatives.
- [`work/`](work/) — active specifications, plans, and progress ledgers. Remove
  a work document when the work finishes after moving any lasting knowledge to
  one of the three directories above.

The workspace architecture entry point is
[`architecture/overview.md`](architecture/overview.md). The active-work tree is
intentionally discoverable from filenames and status headers rather than a
manually duplicated status catalog in this file.

## Relationship to agent skills

Repository development workflows live under [`.agents/skills/`](../.agents/skills/).
The skill is the agent's entry point: it says when the workflow applies, which
context to load, what outcome to produce, and how to verify it.

Shared engineering knowledge remains canonical here. A development skill names
the exact `docs/` file to read when that context is relevant. Do not symlink
files or directories between `docs/` and a skill's `references/`; keep
skill-private material physically inside that skill instead.

When an agent requires a client-specific discovery directory, symlink the
complete skill folder to its canonical `.agents/skills/` location. Those
discovery aliases do not change content ownership.

Shippable user-agent skills live under [`skills/`](../skills/) and must remain
self-contained because installers, plugin manifests, and BitRouter's skills
server distribute that directory independently of these engineering docs.

## Placement test

1. If a fact can be checked mechanically, encode the check instead of prose.
2. If every change must obey it, put the short rule in [`AGENTS.md`](../AGENTS.md).
3. If it is a repeatable procedure, put it in a focused development skill.
4. If only that procedure needs the detail, use the skill's `references/`.
5. If several procedures need the same durable explanation, put it here and
   have each skill load it explicitly.
6. If it teaches users or user agents how to operate BitRouter, put it in
   `bitrouter-docs` or a shippable skill instead.
