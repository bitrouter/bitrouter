# BitRouter Development Guidelines

## Documents

See `README.md` and `DEVELOPMENT.md` for full project introduction and architecture.

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
   their hook/MCP commands invoke `bitrouter` subcommands
   (`status --agent`, `reload`, `mcp serve`) and must never reference
   a CLI surface that doesn't exist.
4. Only **shippable** skills live in `skills/` — that directory is served
   verbatim by the skills install rails and both plugin manifests. Dev-only
   skills go in `.claude/skills/` (auto-loaded for contributors, never
   shipped). Never name any file under `skills/bitrouter/references/`
   `SKILL.md` — Codex's recursive skill scan would load it as a second skill.

## Documentation

Product docs live in `docs/` and are synced to the docs site (`bitrouter-docs`)
at build time. `docs/CONTRIBUTING.md` is the authoring contract (plain Markdown,
no `import`/`export`, only the whitelisted global components, extensionless
internal links).

1. **ALWAYS** keep English and Chinese in lockstep. Every `docs/<section>/<name>.md`
   has a Simplified-Chinese sibling `docs/<section>/<name>.zh.md`. When you add,
   edit, or remove a doc page, make the **identical** change to its `.zh.md` in the
   same change — never ship an English-only or out-of-date translation.
2. A translation mirrors the English page exactly except for prose: preserve every
   code block, component tag (`<Callout>`, `<Tabs>`, …), heading, and link target
   verbatim; translate only human-readable text. Keep `title:` as the English;
   translate `description:`.
3. **NEVER** hand-edit the `sourceHash` frontmatter field — the docs sync manages it
   (it tracks whether a translation is current).
4. **NEVER** author API-reference operation pages or the `ai-resources`/root nav
   here — those are owned by `bitrouter-docs` (see `docs/CONTRIBUTING.md`).
5. **ALWAYS** keep `docs/get-started/supported-models.md` and
   `supported-providers.md` (and their `.zh.md` siblings) consistent with the
   `registry/` catalog whenever you add, remove, or re-scope a vendor or provider
   in `registry/models/` or `registry/providers/`. Two things must stay in sync:
   - **The catalog/directory tables.** These are generated, not hand-edited. They
     live under the `Model catalog` / `Provider directory` anchor heading in all
     four pages and mirror the built registry catalog. Rebuild the registry
     (`cargo run -p dist-helper -- registry build`) and then regenerate the tables
     (`cargo run -p dist-helper -- registry docs`), which rewrites only the
     anchored block in each page. The English and Chinese tables share identical
     data rows — only the header row differs — so run the command rather than
     editing rows by hand. `cargo run -p dist-helper -- check` fails if the
     committed tables are stale.
   - **The surrounding prose.** It hardcodes registry-derived facts the script does
     not touch: the discounted-vs-closed-source vendor families (`gpt-*`,
     `claude-*`, `gemini-*`, `grok-*`), example model ids, and the default discount
     percentage. Update these in the same change so the docs never describe a
     catalog that no longer matches.

## Contributing

1. **ALWAYS** use the **conventional** git commit message format. Keep the title under 60 characters. The message body and footer can be any length.
2. **ALWAYS** use the format of **conventional** git commit message's header part for your PR title. We validate this.

## Run Checks Before Submitting Code

Run these checks before submitting to users if you modified source code:

1. `cargo nextest run --all-features` or `cargo test --all-features` if `cargo-nextest` is absent: Ensure all unit tests, integration tests and doc tests pass.
2. `cargo clippy --all-features`: Ensure you are following Rust's best practices. Direct auto-fix, if applicable: run `cargo clippy --all-features --fix` at a clean git workspace.
3. `cargo fmt -- --check`: Ensure the source code is correctly formatted. Direct auto-fix: run `cargo fmt`
