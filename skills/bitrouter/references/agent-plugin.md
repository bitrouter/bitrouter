# Agent plugin (Claude Code / Codex)

BitRouter ships as an installable **agent plugin** for Claude Code and Codex.
The manifests live at the repo root (`.claude-plugin/`, `.codex-plugin/`) and
reference this skill in place — the plugin, the skill rails, and the CLI ship
from one repo and stay in lockstep.

## What the plugin adds

| Component | Claude Code | Codex |
|---|---|---|
| This skill (`bitrouter`) | ✓ (as `bitrouter:bitrouter`) | ✓ |
| `SessionStart` status line (`bitrouter status --agent`) | ✓ | ✓ |
| Auto-reload on `bitrouter.yaml` edits (`FileChanged` hook) | ✓ | — (no FileChanged event) |
| Live cost-feed monitor (`bitrouter events --follow`) | ✓ (experimental monitors, CC ≥ 2.1.105) | — (no monitor mechanism) |
| Per-turn spend line (`Stop` hook → `bitrouter events --turn --hook codex`) | — (monitor supersedes) | ✓ |
| Origin MCP server (`complete` / `list_models` / `status`) | ✓ auto-starts | ✓ but **must be enabled manually** |

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
end with: "run `bitrouter spawn -a claude` (or `-a codex`), or restart the
harness with the env override, to route this session." The one thing that
works immediately without a restart is the origin MCP server: `complete` can
offload subtasks to cheap models right away.

## Reading the cost feed

- Lines are **aggregated**, never per-request: failure lines (rate-limited to
  one per minute), whole-dollar spend crossings, and a rolling summary at
  most every 10 minutes. Steady-state ≤ 6 lines/hour by design — silence
  means nothing changed.
- `spawn` prints a session spend summary after the wrapped harness exits;
  `status --agent` opens each session with spend-today / this-month when the
  metering DB has data.
- v1 reports **spend**, not savings — the counterfactual "vs frontier list
  price" line lands together with the `bitrouter usage` pricing plumbing.
  Don't promise savings percentages yet.
- All figures are **estimates** from the configured pricing table
  (`estimated_charge_micro_usd`), HUD-grade, not invoice-grade.

## Troubleshooting

- **MCP server shows an error in `/plugin`** → the `bitrouter` binary isn't
  installed yet. Install it (see SKILL.md §2), then `/reload-plugins`.
- **SessionStart says "NOT routed"** → daemon is up but the session env
  doesn't point at it. That's the restart handoff above, not a bug.
- **Cost feed silent** → expected when the session isn't routed through the
  local daemon (e.g. Cloud base URL) — the local metering DB only records
  local daemon traffic.
- **Hooks prompt for trust on install** → expected on both platforms; every
  hook is a read-only one-liner (`status --agent`, `events`, `reload`) and
  survives review.
- **Codex install copies the whole plugin root** into
  `$CODEX_HOME/plugins/cache/…` — from a fresh clone that's the repo
  checkout; from a **dev checkout it includes `target/` and `.git`** (can be
  multiple GB). Dogfooders: install from a clean clone, not your build tree.
