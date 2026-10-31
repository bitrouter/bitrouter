# BitRouter Development Guidelines

## Documents

See [README](README.md), the [documentation policy](docs/DEVELOPMENT.md)
and [workspace architecture](docs/development/architecture.md).

## Guidelines

1. **NEVER** use `#[allow(xxx)]` to bypass checks.
2. **NEVER** re-export components in a public mod. If you already have a public mod: `pub mod a;`, you never re-export components inside it: `pub use a::A; // Don't do this`.
3. **NEVER** use `.unwrap`, `.expect` or `panic!` to make the Rust program panic.
4. **NEVER** over-design types, functions and methods that is never used in the feature or fix you are working on. We don't allow dead code.
5. **Registry data changes do not require TDD.** For changes confined to `registry/`, do not add model- or provider-specific Rust tests that freeze catalog entries or counts. Validate the data with `cargo run -p dist-helper -- registry validate`, `cargo run -p dist-helper -- registry build`, and `cargo run -p dist-helper -- check` instead.
6. Keep executable source and test fixtures free of Python files and Python interpreter requirements. Use Cargo-built Rust fixtures for protocol and process tests. Historical evidence and Python client examples may retain their original language.

## Agent Skill

The `/bitrouter` Agent Skill lives in `skills/bitrouter/` and is the source of
truth for how agents drive BitRouter. It documents facts that drift easily —
the listen port (`127.0.0.1:4356`), env var names (`GEMINI_API_KEY`, not
`GOOGLE_API_KEY`; `OPENCODE_ZEN_API_KEY` shared by Zen and Go), the
`provider/model` slash form, and which CLI subcommands exist.

1. **ALWAYS** update `skills/bitrouter/` in the same change when you alter a CLI
   flag, listen port, env var, default config, or harness wiring step. The skill
   must not describe a CLI that no longer matches `apps/bitrouter`.
2. Keep `skills/bitrouter/SKILL.md` under ~200 lines; deep detail goes in
   `skills/bitrouter/references/`.
3. The same lockstep rule covers the **agent-plugin manifests** at
   `.claude-plugin/`, `.codex-plugin/`, and `.agents/plugins/marketplace.json`:
   they distribute the BitRouter skill and must never reference a CLI surface
   that doesn't exist. The OSS plugin does not register an origin MCP server.
4. Only **shippable** skills live in `skills/` — that directory is served
   verbatim by the skills install rails (`npx skills add` and both plugin
   manifests). Acquisition belongs to `npx skills`; `bro skills` inspects and
   writes explicitly selected local skill directories without an installation
   index, Git downloader, or version store. Never put
   contributor-only tooling
   there. Never name any file under `skills/bitrouter/references/` `SKILL.md`
   — Codex's recursive skill scan would load it as a second skill.

## Documentation

All development and audit agents **MUST** follow the
[documentation policy](docs/DEVELOPMENT.md#documentation-policy).
`docs/specs/` is reserved for deliberately designed and reviewed,
compatibility-sensitive configuration, artifact and API formats. Keep one
canonical specification per object; do not create a spec per feature or
transport. Update schemas/DTOs, examples and compatibility cases together.
Beta status does not waive design or review. Implementation, architecture and
boundary conventions belong in `docs/development/` and evolve with the code.
Retain useful UX and operator instructions.

**Audit references for necessity and correctness.** Preserve or consolidate
necessary format and engineering documents, then update their consumers.
Remove unnecessary references and their process-only surrounding text. Do not
replace a deleted-document link with a Git-history URL just to keep it resolving,
or strip link markup while leaving an unsupported citation. Check paths,
anchors, inline filenames, source comments, generated descriptions, skills and
build/test dependencies. Historical versions are referenced only for an actual
compatibility or migration need. Do not add plans, progress ledgers, dated
evidence or experiment outputs to tracked docs. Product documentation is
authored in `bitrouter-docs`.

1. **The docs site generates its supported-models table** from its committed
   catalog snapshot. Refreshing that snapshot reads the public `/v1/models`
   catalog and uses this repo's committed `dist/registry/models.json` only as
   a fallback for open-weight metadata. There is no generated
   `supported-providers` table; provider discovery is documented in the API
   reference. When you add, remove, or re-scope a registry entry, rebuild and
   commit this repo's catalog (`cargo run -p dist-helper -- registry build`,
   then commit `dist/registry`); `cargo run -p dist-helper -- check` fails if
   it is stale. Refresh the docs catalog in `bitrouter-docs` rather than
   hand-maintaining its generated table.
2. Prose that hardcodes registry-derived facts — the discounted-vs-closed-source
   families (`gpt-*`, `claude-*`, `gemini-*`, `grok-*`), example model ids, the
   default discount — now lives in `bitrouter-docs`; update it there when the
   catalog changes.
3. On each release, an agent in `bitrouter-docs` drafts a docs update from the
   changelog for human review — no docs action is needed in this repo.

## Contributing

1. **ALWAYS** use the **conventional** git commit message format. Keep the title under 60 characters. The message body and footer can be any length.
2. **ALWAYS** use the format of **conventional** git commit message's header part for your PR title. We validate this.

## Run Checks Before Submitting Code

Run these checks before submitting to users if you modified source code:

1. `cargo nextest run --all-features` or `cargo test --all-features` if `cargo-nextest` is absent: Ensure all unit tests, integration tests and doc tests pass.
2. `cargo clippy --all-features`: Ensure you are following Rust's best practices. Direct auto-fix, if applicable: run `cargo clippy --all-features --fix` at a clean git workspace.
3. `cargo fmt -- --check`: Ensure the source code is correctly formatted. Direct auto-fix: run `cargo fmt`
