# Spec: `bitrouter` onboarding — a guided CLI wizard over existing verbs

Status: **proposed — not yet implemented** · Author: Claude (with Spikel) ·
Date: 2026-07-15 · Branch: `claude/cli-onboarding-flow`

v1 is a **deterministic CLI wizard** (scripted prompts, no LLM, no Node, no
TUI-manager dependency) that *sequences verbs that already exist* and ends in
first value — a launched harness or a running daemon. Agent-led onboarding is
deferred (§2). The design was pressure-tested against the current tree
(2026-07-15); the load-bearing constraint below comes from that review.

**Load-bearing constraint — the wizard writes no config.** `Config`
(`crates/bitrouter-sdk/src/config/mod.rs`) is `Deserialize`-only; there is no
serializer and no comment-preserving YAML editor, and the only config write in
the tree is the `STARTER_CONFIG` string template (`apps/bitrouter/src/commands.rs`).
So the only durable state onboarding produces is **credentials** (which already
persist to the credential store, independent of `bitrouter.yaml`, and are
auto-detected by zero-config). Everything that would need a config write —
a remembered default model, a default orchestrator, a harness fleet — is
session-only and **deferred** (§2). This collapses the original six-step vision
to three.

## 1. Motivation

Onboarding today is command-scattered plus passive stderr hints
(`announce_zero_config` / `print_onboarding_hint`, `apps/bitrouter/src/main.rs`).
A fresh user must already know which of `cloud login` / `start` / `providers
login` / `launch` to run; the hint only appears *after* they run something. The
skill even documents the absence: *"BitRouter has no interactive setup wizard —
onboarding is two commands"* (`skills/bitrouter/SKILL.md`). That stance is
agent-native but leaves humans without a front door.

**Design decision (this spec):** add ONE guided entry that *orchestrates the
existing verbs* and preserves the agent-native contract — every prompt is
flag-addressable and `--yes` runs the whole thing non-interactively, emitting
the standard JSON envelope. The wizard is human sugar over the same commands an
agent would call, not a second code path.

Why not agent-led onboarding (an LLM agent driving setup in the TUI): it hits a
**bootstrap paradox** — the onboarding agent routes its own calls through
BitRouter, but acquiring a credential is precisely what onboarding is *for*, so
it cannot emit its first token before the thing it sets up exists. It also puts
npm/Node on the critical path of a self-contained Rust binary. Deferred to a
later track; v1's payoff is the existing `bro launch` (which wraps the
harness's own native TUI).

## 2. Goals / non-goals

Goals (v1):

1. One guided entry: bare `bitrouter` when unconfigured, and `bro init`
   to (re)run it; the wizard ends in first value.
2. Sequence existing verbs only — the sole new durable state is credentials in
   the credential store.
3. Agent-native preserved: every prompt has a flag equivalent; `--yes` runs
   headless and emits the JSON result envelope; `--yes` never blocks on a human.
4. Orchestrator choice restricted to `claude | codex` — the harnesses
   `bro launch` can actually run.
5. First-run detection is **network-free** — deciding whether to onboard must
   not fetch the registry or hit the network.

Non-goals (deferred, tracked):

- Persisting a default model / default orchestrator / harness fleet — needs a
  config writer + schema fields that do not exist (§8). Session-only in v1.
- ACP agents (`gemini-cli`, `pi-acp`, …) as launch orchestrators — `SpawnAgent`
  is `claude | codex` only; ACP agents stay `spawn`-only workers.
- A comment-preserving `bitrouter.yaml` editor / in-file reconfigure.
- Agent-led / TUI-manager onboarding (bootstrap paradox; separate track).
- `--yes` completing interactive OAuth — reported-and-skipped, not attempted (§6).

## 3. CLI surface

### 3.1 Entry points

```
bitrouter                      # no subcommand: run wizard IFF unconfigured; else status + hint
bro init                 # interactive wizard (first run or re-run / reconfigure)
bro init --yes [flags]   # non-interactive: today's behavior — scaffold starter bitrouter.yaml
bro init --force         # allow overwriting an existing bitrouter.yaml when scaffolding
bro init --reset         # clear stored onboarding credentials, then run the wizard
```

- clap change: `Cli.command` becomes `Option<Command>` (`main.rs:58-59`). `None`
  dispatches to `onboarding::entry`, which runs the credential probe (§5) and
  either launches the wizard (unconfigured) or prints status + a one-line hint
  (configured). Bare `bitrouter` never re-onboards a configured user and never
  silently spawns a harness or daemon.
- `init` keeps `-c/--config <path>` and its default write path, and gains
  `--yes`, `--force`, `--reset`. **`bro init --yes` reproduces today's
  `commands::init`** (write `STARTER_CONFIG`; refuse to overwrite unless
  `--force`) — the sensible headless default when there is nothing to ask.
  Interactive `bro init` runs the wizard.
- `announce_zero_config` / `print_onboarding_hint` are superseded for the
  interactive case; the single-line hint is retained for non-TTY / when a user
  runs a specific command without onboarding.

### 3.2 The wizard (three steps)

Each prompt maps to a flag (used by `--yes` and scriptable directly).

**Step 1 — Credentials.** Probe (§5) and *prefill*: show detected env keys, an
active cloud session, and a claude-code session before asking anything.

```
default → bro cloud login          # device-flow OAuth, one account = every model
  or   → bro providers login <id>   # claude-code / openai-codex / github-copilot / supergrok
  or   → paste a BYOK key                 # openai / anthropic / google / openrouter / opencode-*
  or   → use detected key(s)
loop   → "add another provider? [y/N]"
```

Persists to the credential store (cloud credentials file / provider store),
independent of `bitrouter.yaml`; zero-config picks them up. Flags:
`--cloud-login`, `--api-key <brk_…>` (cloud), `--provider <id>` (repeatable)
with `--provider-api-key <k>`, `--use-detected`.

**Step 2 — Harness.** "Which coding agent do you drive?" → `claude | codex`
(multi-install allowed). Install missing binaries via the native installer
(`spawn.rs` `ensure_agent_installed` / `confirm_install`), then **re-resolve the
freshly-installed path** before continuing so the launch exit can't dead-end on
the PATH-after-install caveat. Flags: `--harness claude|codex` (repeatable),
`--no-install`.

**Step 3 — Finish (three-way exit).**

```
(a) launch now      → bro launch -a <harness> [--model <id>]   # native TUI, this-session model
(b) start + snippet → bro start; print paste-in base_url/env for your existing tool
(c) exit            → nothing launched  (+ optional "write a starter bitrouter.yaml? [y/N]")
```

Exit (a) picks `claude|codex` when both are installed; `--model` is handed to
the harness for this session only (not persisted — see §8). Exit (c)'s optional
config write calls the existing `init` writer (the one safe config write).
Flags: `--after launch|serve|exit`, `--model <id>`, `--write-config`.

### 3.3 Result envelope

The wizard (and every `--yes` run) emits the standard JSON result on stdout:

```json
{
  "action": "onboarding",
  "providers_configured": ["bitrouter", "openai"],
  "providers_skipped_interactive": ["github-copilot"],
  "harnesses_installed": ["claude"],
  "after": "launch",
  "snippet": null
}
```

## 4. `--yes` / headless contract

`--yes` runs the whole wizard non-interactively and **never blocks on a human**:

- **Credentials** — consume already-present credentials + flag-supplied keys
  (`--api-key`, `--provider-api-key`). Any provider that would require
  interactive OAuth (cloud device flow without `--api-key`; provider PKCE /
  device-code; the claude-code session import) is **reported-and-skipped** in
  `providers_skipped_interactive`, not attempted. Grounded: cloud login is
  non-interactive only via `--api-key` (`cloud/cli.rs`), and the OAuth/PKCE/
  claude-code flows all require a TTY (`commands.rs`).
- **Harness** — install runs only for explicitly `--harness`-named agents; a
  missing, un-named harness is reported, not installed.
- **After** — defaults to `exit` unless `--after` is given; `launch` is honored
  only when the chosen harness is already present.

This makes the agent-native promise honest: the parts a machine *can* do run;
the parts that inherently need a human are surfaced in the envelope, not hung on.

## 5. Network-free credential probe

`onboarding::probe()` decides "configured vs not" without any network call. It
reads, in order, purely local signals:

1. **BYOK env keys** — `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`
   (not `GOOGLE_API_KEY`), `OPENROUTER_API_KEY`, `OPENCODE_ZEN_API_KEY`.
2. **Cloud session** — the credentials file at
   `$XDG_DATA_HOME/bitrouter/account-credentials.json`.
3. **Subscription sessions** — the provider credential store (e.g. a
   claude-code session), via a local marker check.

It must **not** call `merge_registry_into` or load the registry (the serve-path
`providers.is_empty()` check runs only *after* that merge, so it is unusable
here). "Configured" = at least one of the three present. The probe also handles
the `BITROUTER_HOME`-set-but-missing case that `resolve_config` currently treats
as a hard error (`paths.rs`) — the onboarding entry offers the wizard instead of
erroring.

## 6. Code changes (grounded)

- `apps/bitrouter/src/main.rs` — `Cli.command: Option<Command>` + `None` →
  `onboarding::entry`; `Command::Init` gains `--yes` / `--force` / `--reset`.
- `apps/bitrouter/src/onboarding.rs` (new) — probe (§5), interactive wizard
  (§3.2), headless runner (§4), result report.
- `providers login` — add `--api-key <KEY>` / `--key-stdin` to
  `ProviderAction::Login` (`main.rs:740`) and thread into
  `login_provider_with_options` (`commands.rs:473`) so the API-key method skips
  `prompt_method_choice` / stdin paste. (Closes the flag-parity gap: today the
  API-key path is stdin-only.)
- Install reuse — `spawn.rs` `ensure_agent_installed`; add a post-install path
  re-resolve before the launch exit.
- Snippet — reuse `spawn.rs` `derive_base_url`; template by auth mode
  (local `skip_auth` placeholder / minted `brvk_` key / cloud `brk_`) and client
  shape (`ANTHROPIC_BASE_URL`+`ANTHROPIC_AUTH_TOKEN` / `OPENAI_BASE_URL` / Codex
  `-c` overrides).

## 7. What onboarding does NOT change

- `bro launch` semantics (env-wrap, daemon auto-start, exit summary) —
  reused as-is for exit (a). No new `launch` flags.
- The credential store format and the zero-config auto-detect path — the wizard
  writes credentials the exact way `cloud login` / `providers login` already do.
- `bitrouter.yaml` — never serialized or rewritten; only the canned starter
  template is ever written, and only on explicit request.

## 8. Config schema change

**None.** v1 adds no fields. `default_model`, `default_orchestrator`, and a
harness "selected" marker deliberately do not exist yet — they are the deferred
persistence layer. When built (separate change), they would land as a
serializable, purpose-built sidecar or a new `defaults:` block *with* a config
writer and the `launch`/routing consumers that read them; none of that is in
scope here.

## 9. Migration & lockstep checklist

Per CLAUDE.md rules, in the same change:

- [ ] `skills/bitrouter/SKILL.md` — revise the "no interactive setup wizard —
      onboarding is two commands" line; document bare `bitrouter` + `bitrouter
      init` wizard and `providers login --api-key`.
- [ ] `skills/bitrouter/references/cli.md` — `init` (interactive / `--yes` /
      `--force` / `--reset`), the new onboarding entry, `providers login
      --api-key`.
- [ ] `CLI.md` — `### init` (new interactive behavior + flags), bare-invocation
      note, `providers login` flag.
- [ ] `.claude-plugin/`, `.codex-plugin/`, `.agents/plugins/marketplace.json` —
      verify none reference a changed surface (MCP command is `mcp serve`,
      unaffected; confirm no manifest invokes bare `bitrouter` or `init`).
- [ ] `docs/get-started/*` (+ `.zh.md` siblings) if any page describes the
      run/onboarding sequence.

## 10. Testing

- **Unit** — probe detection is network-free for each of env / cloud-file /
  claude-code, and false when none present; `init --yes` == today's write and
  refuses overwrite without `--force`; `--reset` clears stored credentials;
  snippet templating per auth mode; result-envelope shape including
  `providers_skipped_interactive`.
- **Integration** — bare `bitrouter` unconfigured → wizard entry; configured →
  status, no wizard, no side effects; `BITROUTER_HOME` set but file missing →
  wizard offered, not a hard error; `init --yes` with no creds and no key flags
  → envelope reports zero providers + skipped-interactive and exits non-blocking;
  install → launch path re-resolve.
- **E2E** (bitrouter-e2e-test skill) — fresh `HOME`: `bro init --yes
  --api-key brk_… --harness claude --after exit` → credential stored, claude
  present-or-reported, JSON envelope emitted; then bare `bitrouter` → status,
  not the wizard.

## 11. Acceptance criteria (v1 done)

- [ ] Bare `bitrouter` launches the wizard when and only when the probe reports
      unconfigured; otherwise prints status + hint. Exit code 0 either way.
- [ ] `bro init` runs the wizard; `bro init --yes` reproduces the
      current starter-file write (with `--force` overwrite, `--reset` clear).
- [ ] Every wizard prompt has a working flag equivalent; `--yes` completes
      without blocking and emits the JSON envelope.
- [ ] `providers login --api-key` seeds a BYOK provider non-interactively.
- [ ] Credentials are the only durable output; no path writes or rewrites
      `bitrouter.yaml` except the explicit starter-template write.
- [ ] The three finish exits work; "launch now" reaches a running harness on a
      freshly-installed binary (no re-shell dead-end).
- [ ] The probe performs no network I/O.
- [ ] Lockstep docs (§9) updated; `cargo nextest run --all-features`, `cargo
      clippy --all-features`, `cargo fmt -- --check` clean.

## 12. Decisions log

Resolved with Spikel, 2026-07-15:

1. **CLI-only for v1; agent-led onboarding deferred** — bootstrap paradox +
   Node on the critical path.
2. **Wizard writes no config; credentials-only persistence** — `Config` is
   `Deserialize`-only, no writer exists; default-model/orchestrator/fleet are
   session-only and deferred.
3. **Orchestrator restricted to `claude | codex`** — the harnesses `launch`
   runs; ACP agents stay `spawn`-only.
4. **Six steps compress to three** — a "default" you don't remember is just an
   in-session pick for the launch you're about to do.
5. **Entry: bare `bitrouter` (unconfigured) + `bro init` (re-run)**;
   `init --yes` = today's scaffold-the-file; `--force`/`--reset` added.
6. **`--yes` reports-and-skips interactive OAuth** rather than attempting or
   hanging; `providers login` gains `--api-key`.

## 13. Open questions

1. Bare-configured `bitrouter` output — a compact `status` view, or clap's help?
   (Leaning: one-line status + "run `bro launch`" hint.)
2. Snippet client detection for exit (b) — ask "which client?", or emit all
   three shapes (Anthropic / OpenAI / Codex) and let the user pick? (Leaning:
   emit all three, labeled.)
3. Does `--reset` clear *all* provider credentials or only the cloud session?
   (Leaning: cloud session + a confirm before touching provider creds.)
