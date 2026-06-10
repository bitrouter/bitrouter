# Setup: environment contract & secret-safe key handling

One-time setup. The goal: the controller session can dispatch workers, but the
**API-key value never enters the controller's context or transcript** — the
controller only ever references the *name* `$BITROUTER_API_KEY`, and the shell
expands it inside the child process.

## 1. The environment contract

Workers read these. Set them in the shell that launches your controller session so
they are inherited by the controller and by every `Bash`/`dispatch.sh` subprocess.

| Variable | Meaning | Example |
|---|---|---|
| `BITROUTER_BASE_URL` | BitRouter endpoint, **Anthropic shape, no trailing `/v1`** | `http://127.0.0.1:4356` |
| `BITROUTER_API_KEY` | `brk_*` key, or `unused` for a `skip_auth` local daemon | `brk_…` |
| `BITROUTER_MODEL_CHEAP` | `provider/model` for `--tier cheap` | see [model-tiers.md](model-tiers.md) |
| `BITROUTER_MODEL_STANDARD` | `provider/model` for `--tier standard` | — |
| `BITROUTER_MODEL_FLAGSHIP` | `provider/model` for `--tier flagship` | — |
| `BITROUTER_CHILD_CONFIG_DIR` | Lean `CLAUDE_CONFIG_DIR` for workers | `~/.config/cost-routed-child` |

> **Anthropic-shape base URL.** Claude Code appends `/v1/messages` itself, so the
> base URL omits `/v1`. With a local daemon that is `http://127.0.0.1:4356`; for a
> managed endpoint it is the host with **no** `/v1`. (The OpenAI shape keeps `/v1`
> — don't reuse that value here.)
>
> **Do not export `ANTHROPIC_BASE_URL` globally.** That would redirect the
> controller itself onto BitRouter. Keep the `BITROUTER_*` namespace separate; the
> mapping to `ANTHROPIC_*` happens only inside the worker (see `dispatch.sh`).

## 2. Pasting the key — pick one (no custom tooling required)

### Option A — plain env file (zero dependencies, recommended baseline)

Put the secret in a file the shell sources, **not** committed to git:

```bash
# ~/.config/cost-routed-subagents/env   (chmod 600; add the path to .gitignore)
export BITROUTER_BASE_URL="http://127.0.0.1:4356"
export BITROUTER_API_KEY="brk_…"
export BITROUTER_MODEL_CHEAP="…"
export BITROUTER_MODEL_STANDARD="…"
export BITROUTER_MODEL_FLAGSHIP="…"
```

```bash
# in ~/.zshrc or ~/.bashrc, or run before launching the controller:
[ -f ~/.config/cost-routed-subagents/env ] && . ~/.config/cost-routed-subagents/env
```

The controller agent never reads this file and never prints `$BITROUTER_API_KEY`.

### Option B — direnv (reusable, auto-loads per directory)

```bash
# .envrc  (direnv; add .envrc to .gitignore)
export BITROUTER_BASE_URL="http://127.0.0.1:4356"
export BITROUTER_API_KEY="brk_…"
```

`direnv allow` once; entering the directory loads the env. No custom code.

### Option C — 1Password `op run` (strongest: secret never touches disk)

Store the key in a vault and inject it at launch, so it exists only in process
memory:

```bash
# .env.tpl  (committed; contains references, not secrets)
BITROUTER_BASE_URL=http://127.0.0.1:4356
BITROUTER_API_KEY=op://Private/bitrouter/api_key

op run --env-file=.env.tpl -- claude        # launch the controller
```

## 3. Worker isolation: `--bare` + a clean config home

The isolation that keeps a cheap worker focused comes from **`--bare`**, which
`dispatch.sh` always passes. Per its own help, `--bare` skips hooks, LSP, plugin
sync, auto-memory, background prefetches, keychain reads, and CLAUDE.md
auto-discovery — so the worker is not handed a large session preamble, is not
pushed to invoke skills, and cannot fall back to the controller's keychain
credentials.

`CLAUDE_CONFIG_DIR` then points the worker's **global** config home at a fresh
directory, so it does not read the controller's global `settings.json` or stored
`.credentials.json` either:

```bash
mkdir -p ~/.config/cost-routed-child   # empty is fine; dispatch.sh creates it too
```

Note the scope: `CLAUDE_CONFIG_DIR` only relocates the global `~/.claude` tree.
Project-level `CLAUDE.md`/`.claude/` inside the worktree are governed by `--bare`
(skipped). If you ever drop `--bare`, the worktree's own project config would load —
usually fine, since a worker editing the repo should follow the repo's conventions,
but it means a cheap model would also pick up whatever the project injects.

## 4. Preflight (no value printed)

Confirm wiring without revealing the key or spending tokens:

```bash
# presence only — prints "ok", never the value
[ -n "$BITROUTER_API_KEY" ] && [ -n "$BITROUTER_BASE_URL" ] && echo "env ok" || echo "MISSING env"

# full resolved invocation, key redacted, nothing spawned
# (--task - reads stdin; --model takes a literal so no tier env is needed)
./dispatch.sh --model demo:model --task - --dry-run <<<'demo task'
```

For a live smoke test that does spend a few tokens, dispatch a trivial task on the
`cheap` tier in a throwaway directory and confirm the worker returns `DONE`.
