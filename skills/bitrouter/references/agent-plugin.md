# Agent plugin (Claude Code / Codex)

BitRouter ships as an installable **agent plugin** for Claude Code and Codex.
The manifests live at the repo root (`.claude-plugin/`, `.codex-plugin/`) and
reference this skill in place — the plugin, the skill rails, and the CLI ship
from one repo and stay in lockstep.

## What the plugin adds

The plugin carries only the **two components that work identically on every
harness** — skills and MCP. Hooks are deliberately **not** here: they're the
least portable plugin component (Grok hooks are block-only, Antigravity has a
different event catalog + output schema, and even `SessionStart` output only
surfaces on some harnesses), so anchoring the plugin on a hook would fragment
the exact thing BitRouter exists to unify. Ambient/live/per-turn cost belongs
to BitRouter's own manager surface (`spawn` HUD, then the TUI/GUI cost-HUD),
which is harness-agnostic by construction. See [[agent-plugins-design]] §5.4.

| Component | Claude Code | Codex |
|---|---|---|
| This skill (`bitrouter`) | ✓ (as `bitrouter:bitrouter`) | ✓ |
| Origin MCP server (`complete` / `list_models` / `status`) + cost footer on results | ✓ auto-starts | ✓ but **must be enabled manually** |

Two cost signals ride alongside but are **not plugin components** — they're
`bitrouter` CLI behaviors that exist independent of the plugin: the MCP
tool-result **cost footer** (part of `mcp serve`), and the **`spawn` exit
spend summary** (printed by `bitrouter launch` on any harness it wraps).

## Install

**Claude Code:**

```text
/plugin marketplace add bitrouter/bitrouter
/plugin install bitrouter@bitrouter
```

**Codex:** add the repo as a marketplace source, then install
`bitrouter@bitrouter` from the `/plugins` surface. Bundled MCP servers do
**not** auto-enable on Codex — after install, walk the user through enabling
the `bitrouter` server in the extensions modal, or the `complete`/`status`
tools won't appear.

Local development: `claude --plugin-dir <repo-root>` loads the plugin
directly; `/reload-plugins` picks up edits.

## The restart handoff (say this every time)

Installing the plugin or wiring env vars **cannot reroute the session that is
already running** — harnesses read their base URL at startup. After setup,
end with: "run `bitrouter launch -a claude` (or `-a codex`), or restart the
harness with the env override, to route this session." The one thing that
works immediately without a restart is the origin MCP server: `complete` can
offload subtasks to cheap models right away.

## Reading the cost surface

Cost shows up **on-demand and at the spawn boundary** (no ambient hook):

- **Every origin-MCP `complete` / `status` call:** a cost footer appended to
  the tool result (spend today + request count).
- **Every `spawn` exit:** a one-line session spend summary.

Notes:

- **Live / per-turn / ambient cost is not in the plugin** — no monitor, no
  session hook. That's intentional: hooks don't port across harnesses (§5.4),
  so live cost is the manager surface's job (`spawn` HUD → TUI/GUI).
- v1 reports **spend**, not savings — the counterfactual "vs frontier list
  price" line lands together with the `bitrouter usage` pricing plumbing.
  Don't promise savings percentages yet.
- All figures are **estimates** from the configured pricing table
  (`estimated_charge_micro_usd`), HUD-grade, not invoice-grade.

## Troubleshooting

- **MCP server shows an error in `/plugin`** → the `bitrouter` binary isn't
  installed yet. Install it (see SKILL.md §2), then `/reload-plugins`.
- **MCP cost footer empty** → expected when the session isn't routed through
  the local daemon (e.g. Cloud base URL) — the local metering DB only records
  local daemon traffic.
- **`complete`/`status` tools missing on Codex** → the bundled MCP server
  wasn't enabled after install (Codex doesn't auto-enable them).
- **Codex install copies the whole plugin root** into
  `$CODEX_HOME/plugins/cache/…` — from a fresh clone that's the repo
  checkout; from a **dev checkout it includes `target/` and `.git`** (can be
  multiple GB). Dogfooders: install from a clean clone, not your build tree.
