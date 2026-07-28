# BitRouter Development Guidelines

## Documents

See `README.md` and `docs/DEVELOPMENT.md` for full project introduction and architecture.

## Guidelines

1. **NEVER** use `#[allow(xxx)]` to bypass checks.
2. **NEVER** re-export components in a public mod. If you already have a public mod: `pub mod a;`, you never re-export components inside it: `pub use a::A; // Don't do this`.
3. **NEVER** use `.unwrap`, `.expect` or `panic!` to make the Rust program panic.
4. **NEVER** over-design types, functions and methods that is never used in the feature or fix you are working on. We don't allow dead code.

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
   their MCP command invokes a `bitrouter` subcommand (`mcp serve`) and
   must never reference a CLI surface that doesn't exist.
4. Only **shippable** skills live in `skills/` — that directory is served
   verbatim by the skills install rails (`bitrouter skills add`, `npx skills
   add`) and both plugin manifests, so never put contributor-only tooling
   there. Never name any file under `skills/bitrouter/references/` `SKILL.md`
   — Codex's recursive skill scan would load it as a second skill.

## Documentation

Product docs live in the **`bitrouter-docs`** repo (`content/docs/`), where they
are authored, reviewed, translated, and published. The authoring contract,
English/Chinese lockstep, and `sourceHash` tracking live there now — not here.
`docs/` in this repo now holds internal **development** docs — the CLI reference
(`docs/CLI.md`), the workspace architecture guide (`docs/DEVELOPMENT.md`), and
design specs (`docs/*_SPEC.md`, `docs/*_ACCEPTANCE.md`); see `docs/README.md`.

1. **The docs site generates the model/provider tables** from this repo's
   committed `dist/registry/{models,providers}.json`. When you add, remove, or
   re-scope a model or provider under `registry/`, rebuild and commit the catalog
   (`cargo run -p dist-helper -- registry build`, then commit `dist/registry`);
   `cargo run -p dist-helper -- check` fails if it is stale. The docs site's
   `supported-models` / `supported-providers` tables regenerate from it
   automatically — do **not** try to hand-maintain those tables (they no longer
   live in this repo).
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
