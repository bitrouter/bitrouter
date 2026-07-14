# Spec: `bitrouter spawn` — ACP sub-agents, routed through BitRouter by default

Status: **implemented (v1) — all open questions resolved (§14)** · Author:
Claude (with Spikel) · Date: 2026-07-13

**Implementation note (2026-07-13).** v1 landed: unified `harness.rs` catalog
(replaces `spawn::AgentSpec` + `agents::CATALOG`), routing injection on the ACP
spawn path (default on, `--direct` opt-out), fail-fast daemon/auth checks
before side effects, `session`/`error` NDJSON lines, `spawn_check` preflight,
and the `launch` (interactive) / `spawn <agent>` (ACP sub-agent) verb split
with `spawn --agent` deprecated → `launch`. `acp serve|prompt` kept as stable
aliases (route by default). Deferred as specced: per-agent `routing: direct`
config field (§9, v1.1 — `--direct` covers v1); pi-acp `ConfigDir` routing
(§6.4, v1.1); session-scoped virtual-key minting + record-id-before-spawn
reorder + observability join (§10, v1.5); MCP spawn tools (§11). All 1454
workspace tests + 6 ACP integration tests pass; clippy + fmt clean.
Supersedes the CLI framing of #672; builds on #613 (per-session ACP substrate).

## 1. Motivation

BitRouter has two agent-launching surfaces that grew independently:

- `bitrouter spawn` — launches an **interactive native-TUI harness** (Claude
  Code, Codex) env-wrapped so its LLM traffic routes through the daemon.
  Encodes per-harness routing knowledge (`ANTHROPIC_BASE_URL` /
  `ANTHROPIC_AUTH_TOKEN`, Codex `-c` provider overrides), daemon auto-start,
  install-on-missing, auth precedence, exit cost summary.
- `bitrouter acp serve|prompt` — launches a **headless ACP session** via the
  substrate (records, worktrees, transcripts, OTel spans, warm reattach), but
  its LLM traffic goes wherever the harness's own config points — **not**
  through the daemon (#672).

The duplication is at the knowledge layer (three per-harness stores:
`spawn::AgentSpec`, `agents::CATALOG`, the ACP registry — and #672 would add a
fourth). The runtimes are *not* duplicates: a transparent TUI launcher and a
resident ACP broker are different process topologies.

**Design decision (this spec):** split by *role*, not by protocol.

| Role | Verb | Protocol | UI | Routed via daemon |
|---|---|---|---|---|
| Main orchestrator agent | `bitrouter launch` | none (native) | harness's own TUI | yes (already default) |
| Sub-agent | `bitrouter spawn` | ACP | none (managed by caller) | **yes (new default)** |

An orchestrator has no manager — the human drives it through its own TUI, so
it doesn't need ACP. Sub-agents are driven by a program (the orchestrator via
CLI/NDJSON, a GUI via `--serve`, the future `bitrouter tui`), so ACP is their
native substrate. Every level of the tree routes generations through the
daemon:

```
human
 └─ bitrouter launch claude            # native TUI, env-wrapped
     └─ (agent runs) bitrouter spawn codex -p "…"   # ACP session, worktree, NDJSON
         └─ bitrouter spawn gemini -p "…"           # ACP all the way down
```

Because BitRouter owns the sub-agent's entire lifecycle, routing-by-default
carries none of the objections raised against retrofitting it onto `acp
serve` (no pre-existing behavior to break, no silent billing change for an
already-configured workflow) — the opt-out remains for the exceptions.

## 2. Goals / non-goals

Goals (v1):

1. `bitrouter spawn <agent>` spawns any catalog-known or config-declared ACP
   harness as a substrate session.
2. Spawned sub-agents' LLM traffic routes through the local daemon **by
   default**, with a per-invocation and per-agent opt-out.
3. CLI/NDJSON is the orchestrator interface (`spawn <agent> -p "…"` streams
   NDJSON; `spawn <agent> --serve` speaks ACP over stdio for GUIs).
4. One harness catalog feeds `launch`, `spawn`, and `agents install`.
5. Preflight (`spawn <agent> --check`) validates daemon liveness and model
   routing before anything launches.

Non-goals (v1, tracked as follow-ups):

- MCP tool surface for spawn (post-v1; see §11).
- Permission relay from sub-agent to orchestrator (headless deny-all stands).
- `bitrouter tui` integration (#604) — consumes this work, not part of it.
- Generation↔session span join — session-scoped virtual keys, with header
  injection as the `skip_auth` fallback (§10, v1.5).
- Gemini/pi routing enablement beyond what §6 verifies.

## 3. CLI surface

### 3.1 `bitrouter launch` (rename of today's `spawn`)

```
bitrouter launch --agent claude [--base-url URL] [--no-install] [--no-start] [--check] [-- <args>]
```

Behavior is exactly today's `bitrouter spawn` (env-wrap, daemon ensure,
install offer, exit cost summary, exit-code propagation). Only the name
changes. `bitrouter spawn --agent <claude|codex>` remains as a **deprecated
alias for two alpha releases** (it is unambiguous: the old form requires
`--agent` with the closed enum; the new form takes a positional id), emitting
a one-line deprecation notice on stderr pointing at `launch`.

### 3.2 `bitrouter spawn` (new: sub-agent creation, subsumes `acp serve|prompt`)

```
bitrouter spawn <agent> -p "<text>" [--no-wait]        # one-shot prompt → NDJSON on stdout
bitrouter spawn <agent> --serve [--warm [--idle-timeout SECS]]   # ACP over stdio (GUI / manager)
bitrouter spawn <agent> --check                        # preflight only, no launch

# shared session flags (carried over from `acp serve|prompt` unchanged):
  [--worktree NAME [--rm-worktree]] [--no-transcript] [--turn-timeout SECS] [-c CONFIG]

# routing flags (new):
  [--direct]              # do NOT inject routing env — agent talks to its provider directly
  [--model MODEL]         # pin the harness's model via its model env var (catalog-known only)
```

- `<agent>` resolves through the **unified catalog resolution chain** (§4):
  config `agents:` entry → compiled catalog → error listing both.
  A catalog-known id that has no config entry is launched from its catalog
  invocation directly — no YAML edit required to spawn a blessed harness.
- Exactly one of `-p` / `--serve` / `--check` is required. A bare
  `bitrouter spawn <agent>` at a TTY errors with a hint (this is the slot
  `bitrouter tui` fills later; we do not TTY-sniff a mode).
- `bitrouter acp serve|prompt` remain as **hidden aliases** delegating to the
  new code path (the GUI's AcpFeed and existing docs keep working); `acp
  sessions` and `acp attach` are unchanged and stay under `acp` (they operate
  on records, not launches).

### 3.3 NDJSON contract

Unchanged from `acp prompt` (self-describing update lines +
`{"type":"result","stop_reason":…}` terminal line; `{"type":"submitted"}`
under `--no-wait`). One addition: when routing is active, the first line is

```json
{"type":"session","record_id":"…","agent":"codex-acp","via":"http://127.0.0.1:4356"}
```

so an orchestrator can correlate the session's record with the cost/metering
it later queries, without parsing stderr. With `--direct`, `"via"` is `null`.

Preflight failures (§8) are also structured: a single
`{"type":"error","code":"daemon_unreachable"|"auth_required",…}` line and a
non-zero exit, emitted before any session side effect. An orchestrator can
therefore branch on `type` of the first line: `session` → the spawn is live;
`error` → nothing was created.

## 4. Unified harness catalog

New module `apps/bitrouter/src/harness.rs` (name bikesheddable) replacing the
per-harness knowledge in both `spawn::SpawnAgent::spec()` and
`agents::CATALOG`:

```rust
pub struct Harness {
    pub id: &'static str,                  // "claude", "codex", "gemini", "pi"
    pub interactive: Option<Interactive>,  // native-TUI facet (launch)
    pub acp: Option<AcpInvocation>,        // ACP facet (spawn)
    pub routing: Routing,                  // how to point LLM traffic at a gateway
}

pub struct Interactive { pub binary: &'static str, pub installer: InstallCommand }
pub struct AcpInvocation { pub command: &'static str, pub args: &'static [&'static str] }

pub enum Routing {
    /// Env-var redirection (claude-code-acp, gemini-cli).
    Env {
        base_url_env: &'static str,
        /// Var carrying the gateway credential, plus which header the
        /// harness turns it into (`Authorization: Bearer` vs a provider
        /// header like `x-goog-api-key`) — `--check` warns when the daemon's
        /// auth mode can't accept that header (§6).
        auth_env: &'static str,
        model_env: Option<&'static str>,
        /// Additional fixed vars required for the redirect to take effect.
        extra: &'static [(&'static str, &'static str)],
        /// Which daemon inbound endpoint this harness will hit
        /// (messages | chat_completions | responses | generate_content) —
        /// used by `--check` to resolve routes against the right protocol.
        protocol: InboundProtocol,
    },
    /// Args injection: append `-c key=value` one-shot overrides to the
    /// invocation (codex-acp forwards argv verbatim to codex core).
    CodexArgs,
    /// Synthesized config dir: write a per-session provider config and point
    /// the harness at it via an env var (pi: `PI_CODING_AGENT_DIR` +
    /// `models.json`). v1.1 — see §6.4.
    ConfigDir { dir_env: &'static str },
    /// Known harness, no gateway mechanism (verified absent).
    Unroutable { reason: &'static str },
}
```

Resolution chain for `spawn <agent>`:

1. **Config entry match** — `agents.<id>` in `bitrouter.yaml` wins; its
   invocation is used verbatim. Its *routing* is resolved by **invocation
   matching**: if `command` + any arg contains a catalog harness's package
   marker (e.g. `@zed-industries/claude-code-acp`), that harness's `Routing`
   applies. YAML key names are user-chosen and must not carry semantics.
2. **Compiled catalog** — id matches a `Harness` with an `acp` facet.
3. Error listing configured ids + catalog ids.

Existing verbs re-read from this module: `launch` uses the `interactive`
facet; `agents install` renders stubs from the `acp` facet; `agents list`
merges as today. `spawn::AgentSpec` and `agents::KnownAgent` are deleted
(guideline 4: no dead code left behind).

## 5. Routing injection (the default)

When `spawn` launches a session and routing is not disabled:

1. **Base URL** — reuse `derive_base_url(cfg.server.listen)` (wildcard→
   loopback, default port 4356). `--base-url` overrides, same as `launch`.
2. **Daemon ensure** — reuse `ensure_local_daemon` (probe control socket,
   auto-start detached `serve`, wait for readiness). Unlike `launch`, spawn
   **fails fast** when the daemon is definitively unreachable after
   auto-start is exhausted (§8) — a routed sub-agent without a daemon is a
   guaranteed-dead session, and the caller is a program, not a watching
   human. The probe runs **before** any session side effect (worktree,
   record, transcript). An ambiguous probe (reachable but the control
   exchange errored) counts as up, same as `ensure_local_daemon`'s existing
   stance, so only a definitively-dead endpoint blocks a spawn.
3. **Injection**, per the harness's `Routing` variant: an env overlay
   (`Env`, `ConfigDir`), or `-c` args appended after the config-declared
   args (`CodexArgs` — codex parses repeated `-c` last-wins, preserving the
   same precedence). Env overlay precedence (later wins):

   ```
   inherited process env          (lowest — poisoned by nested launches)
   config `agents.<id>.env`       (user-authored, explicit)
   routing injection              (highest — owns its keys unconditionally)
   ```

   The injection *always* sets `base_url_env` and `auth_env` — never
   inherits them through — because in this topology **every** spawn runs
   inside an env-wrapped `launch` parent, and inheriting the parent's
   `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN` is the norm, not the edge
   case. If the config `env:` block already sets a key the injection owns,
   the injection still wins but a one-line stderr warning names the
   collision (the user said two contradictory things; the flag-less default
   sides with routing, `--direct` sides with their env block).
4. **Auth resolution** — a single resolver function decides the credential
   injected into the harness's auth var (one place to change when v1.5
   minting lands, §10):
   - `BITROUTER_API_KEY` (`brk_…`) when exported → use it.
   - Else, local daemon with `skip_auth: true` (the `bitrouter init`
     default) → `"bitrouter-local"` placeholder.
   - Else — local daemon with `skip_auth: false`, or a remote `--base-url`
     whose config spawn cannot read — **hard error** before launch:
     `daemon requires auth; export BITROUTER_API_KEY or create a key`.
   - v1.5: for the local daemon under auth, mint a session-scoped virtual
     key (`brvk_…`) over the control socket instead of erroring (§10).

   The token goes in the harness's *bearer* var (e.g.
   `ANTHROPIC_AUTH_TOKEN`), never the provider-native key var (e.g.
   `ANTHROPIC_API_KEY` → `x-api-key`, which is not BitRouter's inbound
   scheme). This corrects the example in #672.
5. **Model pin** — `--model X` sets the harness's `model_env` (error if the
   harness has none). Without it the harness's default model ids must
   resolve in the routing table; `--check` verifies this (§7).

Opt-outs:

- `--direct` per invocation.
- `agents.<id>.routing: direct` in config (new optional field, default
  `auto`). `auto` = inject when the harness is catalog-matched and routable;
  warn-and-launch-direct when unknown or `Unroutable` (spawn must still work
  for custom agents — refusing would make the default a regression).

## 6. Per-harness routing matrix

Verified against adapter source (claude-code-acp v0.58.1, codex-acp @ codex
rust-v0.137.0, gemini-cli ≥ v0.50, pi-acp + earendil-works/pi), 2026-07.

| Harness | Mechanism | Gateway auth → header | Model pin | Daemon inbound | Phase |
|---|---|---|---|---|---|
| claude-acp | `ANTHROPIC_BASE_URL` env (adapter passes `process.env` through to the SDK-spawned CLI) | `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer` (also suppresses login) | `ANTHROPIC_MODEL` | `/v1/messages` ✅ | v1 |
| codex-acp | `CodexArgs`: append `-c model_provider=bitrouter -c model_providers.bitrouter.base_url=…/v1 …` (npm launcher forwards argv verbatim) | `-c model_providers.bitrouter.env_key=BITROUTER_API_KEY` → `Authorization: Bearer`; custom provider bypasses codex login entirely | `-c model=…` | `/v1/responses` ✅ (**required**: pinned codex 0.137 removed `wire_api="chat"`) | v1 |
| gemini-cli | `GOOGLE_GEMINI_BASE_URL` env — auto-selects the GATEWAY auth type in ACP mode | `GEMINI_API_KEY` → **`x-goog-api-key`** (see caveat) | `GEMINI_MODEL` | `/v1beta/models/{model}:streamGenerateContent` ✅ | best-effort |
| pi-acp | `ConfigDir`: synthesize `models.json` (provider `baseUrl` + `api` + `apiKey: "$BITROUTER_API_KEY"`, `authHeader: true`) in a per-session dir, set `PI_CODING_AGENT_DIR` | models.json `authHeader: true` → `Authorization: Bearer` | `settings.json` default in the same dir | any (`api:` selects messages/completions/responses) | v1.1 |

### 6.1 claude-acp notes

Cleanest of the four — identical knobs to interactive Claude Code, so the
catalog entry shares its routing spec with the `launch` facet's. The adapter
also honors `ANTHROPIC_CUSTOM_HEADERS` (newline-separated `Name: value`),
which §10 uses. Resume caveat: on `session/load`, the transcript's model
wins over `ANTHROPIC_MODEL`.

### 6.2 codex-acp notes

Reuses `launch`'s codex wiring almost verbatim (`build_codex_child_launch`
generalizes: same `-c` strings, same TOML escaping, same env_key vs
`experimental_bearer_token` split on whether `BITROUTER_API_KEY` is set).
Injected args are **appended after** the config-declared args; codex parses
repeated `-c` with last-wins, so injection beats a user's conflicting
override — consistent with §5.3 precedence. `--check` reuses
`codex_route_check` (route must include a `responses`-capable hop).
Alternative rejected: `CODEX_HOME` synthesized dir — heavier, and it would
*mask* the user's `~/.codex` auth/config rather than layer on it.

### 6.3 gemini-cli — best-effort only (harness deprecated upstream)

Google is deprecating gemini-cli in favor of Antigravity, which has no
official ACP support yet. The env mechanism above is verified and costs one
catalog entry, so it ships as **best-effort**: injected when matched, no
daemon-side work planned for it. Concretely:

- **Auth header mismatch stands**: gemini sends `x-goog-api-key`; the
  daemon's inbound auth hook accepts only `Authorization: Bearer` and
  `x-api-key` (`auth/hook.rs`). Gemini therefore routes only under the
  `skip_auth: true` default; `--check` warns under auth mode. Extending the
  auth hook is **not planned** (decision 2026-07-13).
- **Settings-pinned auth types override env** (`security.auth.selectedType`
  ignores `GOOGLE_GEMINI_BASE_URL`); injection sets `GEMINI_CLI_HOME` to a
  per-session dir to isolate. No version pinning beyond the catalog's
  invocation; regressions surface via `--check`.
- When Antigravity ships ACP support it enters the catalog as a new harness.
  (Daemon-side Antigravity BYO-subscription support already exists — #678 —
  but that is the provider plane, independent of this spec.)

### 6.4 pi-acp (v1.1)

pi has **no base-URL env var** (provider base URLs are hardcoded); the only
non-invasive mechanism is a synthesized agent dir (`PI_CODING_AGENT_DIR`)
containing `models.json` + `settings.json`. That means generating provider
config with a model list at spawn time — more machinery than env overlay, so
it ships as a fast-follow. Until then pi spawns with a
`routing unavailable — synthesize PI_CODING_AGENT_DIR manually` warning
(§8), i.e. today's behavior, not a regression.

> **Shipped for the interactive facet** (`Routing::PiConfigDir`,
> `Harness::orchestrator_overlay`): the `bitrouter tui` orchestrator and
> attach synthesize `models.json` (model list from the daemon's
> `/v1/models`) and select `--provider bitrouter --model <id>`; the model
> default rides the CLI flag rather than a `settings.json`. The same
> mechanism also routes **opencode** (`Routing::OpencodeConfig`, one
> synthesized `OPENCODE_CONFIG` JSON carrying provider + default model +
> MCP). Headless `spawn` still launches both direct with a note — wiring
> the synthesis into the ACP facet remains the v1.1 follow-up.

### 6.5 ACP-native gateway auth (noted for phase 2)

claude-code-acp and gemini-cli both expose a first-class ACP `gateway` auth
method: the *client* advertises a capability at `initialize` and passes
`_meta.gateway: { baseUrl, headers }` on `authenticate`. BitRouter's
substrate **is** the ACP client, so it could deliver base URL + per-session
headers (including the §10 join header) over the protocol instead of env —
cleaner, adapter-blessed, and per-session rather than per-process. Not v1:
it covers only two of four harnesses and requires substrate `authenticate`
plumbing; env/args injection is the uniform baseline it layers onto. One
side effect to respect: gemini persists the ACP-selected auth type into user
settings — another reason `GEMINI_CLI_HOME` isolation stays.

## 7. Preflight (`spawn <agent> --check`)

Mirrors `launch --check`, adapted:

| Check | Pass condition |
|---|---|
| invocation | `npx`/`uvx`/binary present on PATH |
| daemon | `GET {base}/health` 2xx (skipped under `--direct`) |
| routing | harness catalog-matched and `Routing` ≠ `Unroutable` (warn, not fail, when direct) |
| model route | `resolve_route` succeeds for `--model` or the harness's known default ids, against the harness's inbound protocol (e.g. Codex requires a `responses`-capable hop — reuse `codex_route_check`) |
| env collisions | config env keys shadowed by injection → warn |

Exit non-zero on any fail, same reporting shape as `SpawnCheckReport`.

## 8. Failure modes

- **Daemon down (routing active)**: auto-start is the first line of defense
  (`ensure_local_daemon`, unchanged). If the daemon is still definitively
  unreachable — auto-start failed, `--no-start`, or a remote `--base-url`
  nobody can start — spawn **fails fast, before any side effect** (no
  worktree, no record, no transcript, no npx fetch):
  - `-p` mode: the only stdout line is
    `{"type":"error","code":"daemon_unreachable","via":"http://127.0.0.1:4356","hint":"bitrouter start, or pass --direct"}`,
    exit non-zero.
  - `--serve` mode: exit non-zero with the message on stderr **before
    speaking any ACP** — a manager handles "child failed to start" far more
    gracefully than a session that initializes and dies on its first turn.
  - `--direct` skips the probe entirely; the hint names it so a user who
    intended the harness's own auth learns the right fix.

  Rationale for diverging from `launch`'s never-block stance: `launch` has a
  human watching who reacts to a visible TUI error in seconds; `spawn`'s
  caller is a program that would otherwise have to fish a harness-specific
  connection error out of `message_chunk` lines, after paying session-setup
  side effects for a session known to be dead. The failure contract follows
  the caller.
- **Auth required, no credential** (§5.4): same fail-fast shape,
  `{"type":"error","code":"auth_required",…}`, before side effects.
- **Model id doesn't resolve**: daemon returns its normal routing error; the
  NDJSON stream carries the harness's surfaced error. `--check` exists so
  orchestrators can preflight once per config, not per spawn.
- **Unknown harness**: warn `routing unavailable for '<id>' (not
  catalog-matched); launching direct — add env to agents.<id>.env to route
  manually` and continue.

## 9. Config schema change

```yaml
agents:
  my-codex:
    name: my-codex
    routing: auto        # auto (default) | direct
    transport:
      type: stdio
      command: npx
      args: ["-y", "@zed-industries/codex-acp@latest"]
      env: {}            # user env — injection overlays on top (see §5.3)
```

One new optional field, `routing`, on `AcpAgentConfig`. No breaking schema
change; absent = `auto`. `agents install <id>` stubs gain a commented
`# routing: auto` line documenting the default rather than baked env
(baked `ANTHROPIC_BASE_URL:` stubs freeze launch-time facts — port, auth
mode — and go stale; the semantic field resolves them at spawn).

## 10. Observability join (v1.5, design constraints now)

The substrate emits `invoke_agent`/`execute_tool` spans keyed by
`gen_ai.conversation.id = record_id`; the daemon meters generations. Today
they correlate only by time window. Two join mechanisms, in order of
preference:

1. **Session-scoped virtual keys (primary, v1.5).** Spawn mints a `brvk_…`
   key over the local daemon's control socket, records the
   key ↔ `record_id` mapping at mint time, injects it via the §5.4
   resolver, revokes it at session shutdown (TTL expiry as the kill-9
   backstop). Every generation the daemon serves is then attributable to
   its sub-agent session **for all harnesses** — the credential is
   universal where custom headers are not (codex and pi have no header
   mechanism). The same key object is the natural carrier for later
   per-sub-agent policy (`spawn --budget …`). Limitation: control-socket
   minting works only for the local daemon we own; remote daemons stay on
   provided `brk_` keys.
2. **Custom-header injection (fallback).** For `skip_auth: true` sessions
   (placeholder credential — nothing to attribute by) on harnesses that
   support it: `ANTHROPIC_CUSTOM_HEADERS: x-bitrouter-conversation:
   <record_id>` (claude-code-acp, verified in adapter source); the ACP
   `gateway` auth method (§6.5) extends this to gemini. Daemon-side, the
   header is stamped onto generation spans + metering rows.

Constraints v1 must satisfy so v1.5 bolts on cleanly: (a) mint `record_id`
**before** the child spawns — today `UpstreamConnection::spawn` precedes the
mint in `engine.rs` (spawn at build step, mint after); (b) thread the
env/args overlay through `LaunchOptions` (the apps layer computes it; the
substrate stays routing-agnostic); (c) route all credential choice through
the single §5.4 resolver. (a) and (b) are required for §5 anyway.

## 11. MCP follow-up (out of scope, direction locked)

`bitrouter mcp serve` later gains `spawn_subagent` / `prompt_session` /
`session_status` tools over the same launch path, adding what CLI/NDJSON
cannot: relaying sub-agent permission requests to the orchestrator as tool
results instead of headless deny-all. Nothing in v1 may assume the caller is
a shell (hence the structured `session` NDJSON line, not stderr prose).

## 12. Migration & lockstep checklist

Per CLAUDE.md rules, in the same change:

- [x] `skills/bitrouter/references/cli.md` (Harness launch & spawn +
      routing), `harness-claude-code.md`, `harness-codex.md`,
      `agent-plugin.md`, `sessions.md` — `launch` rename, routing default,
      `--direct`, `session`/`error` NDJSON lines.
- [x] `.claude-plugin/`, `.codex-plugin/`, `.agents/plugins/marketplace.json`
      — verified: no manifest references `spawn` (MCP command is `mcp serve`,
      unaffected).
- [x] `CLI.md` (`### launch` + new `### spawn`), `docs/integrations/codex.md`
      + `claude-subscription.md` and their `.zh.md` siblings, in lockstep
      (interactive `spawn` → `launch`).
- [ ] GUI `AcpFeed` — no repoint needed (aliases keep `acp serve` alive);
      optionally handle the new `session` NDJSON first line if the GUI parses
      prompt output. (Separate repo — not in this change.)
- [x] Deprecation: `spawn --agent` prints a migration note and dispatches to
      `launch` (removal target = one or two alpha releases). `acp
      serve|prompt` kept as stable aliases (route by default), not removed.

## 13. Testing

- **Unit**: env overlay precedence (inherited < config < injection);
  invocation matching (config entry → catalog routing); credential resolver
  (key → placeholder → hard error by auth mode);
  `Routing::Unroutable` warn-and-direct path; NDJSON `session` first line;
  structured `error` line shape for `daemon_unreachable` / `auth_required`.
- **Integration (fail-fast ordering)**: daemon definitively down +
  `--no-start` → spawn exits non-zero with the single `error` NDJSON line
  and **no** worktree/record/transcript is created (assert `.bitrouter/`
  untouched); `--direct` under the same conditions launches.
- **Integration** (`apps/bitrouter/tests/acp.rs` pattern): stub ACP agent
  script that echoes its env → assert injected vars present and overriding
  a poisoned inherited `ANTHROPIC_BASE_URL`; `--direct` asserts absence.
- **E2E** (bitrouter-e2e-test skill): `spawn claude-acp -p "…"` against a
  live daemon with a mock upstream; assert the generation hits the daemon
  (metering row exists for the session window) and NDJSON terminates with
  `result`.

## 14. Decisions log

All open questions resolved 2026-07-13:

1. **Verb names**: `launch` (orchestrator, native TUI) / `spawn`
   (sub-agent, ACP) as specced.
2. **`spawn --serve`** is the documented form; `acp serve|prompt` become
   hidden aliases.
3. **Daemon-down**: fail fast with a structured NDJSON/stderr error before
   any session side effect, after `ensure_local_daemon` auto-start is
   exhausted (§8). Diverges deliberately from `launch`'s never-block stance
   — the failure contract follows the caller (program vs human).
4. **Auth under `skip_auth: false`**: v1 errors with a pointer to
   `BITROUTER_API_KEY` via the single credential resolver (§5.4);
   session-scoped virtual-key minting is v1.5, designed jointly with the
   observability join (§10) which it primarily serves.
5. **gemini-cli**: best-effort only — deprecated upstream in favor of
   Antigravity (no official ACP support yet); revisit when Antigravity
   ships ACP (§6.3).
6. **`x-goog-api-key` inbound auth support**: not planned.
