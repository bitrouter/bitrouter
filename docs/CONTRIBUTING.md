# Contributing to the docs

The published documentation site ([bitrouter.ai/docs](https://bitrouter.ai/docs))
is rendered from this `docs/` folder. The site repo (`bitrouter-docs`) syncs this
tree at build time — so docs live next to the code they describe and ship in the
same PR.

## What publishes

Each top-level folder here (`get-started`, `concepts`, `features`, `guides`,
`integrations`) is a documentation section synced to the site. Internal folders
(`superpowers/`, `awesome-submissions/`) are excluded. Page order within a section
is the `pages` list in that section's `meta.json`.

The overall section order and the **site-only** sections — the Cloud **API
reference** (generated from the BitRouter Cloud OpenAPI spec) and **AI resources**
(docs MCP, llms.txt, drop-in skills) — live in the `bitrouter-docs` repo, which
renders this tree. Don't author those here.

## Authoring contract (plain Markdown)

Pages are plain Markdown (`.md`), not MDX with imports. The sync enforces this:

1. **Frontmatter** — every page needs `title:` (and ideally `description:`). A
   `sourceHash:` is managed automatically; don't hand-edit it.
2. **No `import` / `export` lines.** A whitelisted set of components is available
   globally without imports: `Callout`, `Tabs`/`Tab`, `Cards`/`Card`, and (on the
   relevant pages) `ModelsTable`, `CalInline`. Any other `<Capitalized>` tag fails
   the sync.
3. **Callouts** — prefer GitHub-style `> [!NOTE]` / `> [!WARNING]` blockquotes, or
   the `<Callout>` component.
4. **Internal links** are site paths without extensions: `/docs/features/byok`,
   not `./byok.md`.
5. **Translations** — a Chinese page is `<name>.zh.md` beside the English
   `<name>.md`. If you change an English page without updating its `.zh.md`, the
   sync flags the translation as stale (it won't block, but it's visible).

## Adding a page

1. Create `docs/<section>/<name>.md` (and `<name>.zh.md` if translating).
2. Add `<name>` to that section's `meta.json` `pages` list in the position you
   want it to appear in the nav.
3. Open a PR. The docs site picks it up automatically once merged to `main`.
