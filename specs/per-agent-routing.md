# Per-Agent Routing Configuration

This document describes how BitRouter redirects each ACP agent's LLM traffic
through its proxy using agent-specific routing metadata.

Related: [GitHub issue #300](https://github.com/bitrouter/bitrouter/issues/300)

## 1. Problem

ACP agents each have their own way of configuring LLM endpoints — some use
env vars (`OPENAI_BASE_URL`, `ANTHROPIC_HOST`), others use config files
(`config.toml`, `settings.json`). The previous approach of writing generic
env vars to the shell profile is fragile: most agents ignore them, it
persists when BitRouter is off, and each agent is a snowflake.

## 2. Solution

A **declarative routing section** in each agent's YAML definition describes
how to configure that agent for BitRouter. Two mechanisms are supported:

- **Env var injection** at subprocess spawn time (TUI connects to agent).
- **Config file patching** during onboarding (`bitrouter init`).

## 3. YAML Schema

Each agent YAML may include an optional `routing` block:

```yaml
routing:
  # Env vars injected into the agent subprocess at spawn time.
  env:
    OPENAI_BASE_URL: "${BITROUTER_URL_V1}"
    OPENAI_API_KEY: "${OPENAI_API_KEY}"

  # Native config files patched during onboarding.
  config_files:
    - path: "~/.codex/config.toml"
      format: toml            # json | toml
      values:
        openai_base_url: "${BITROUTER_URL_V1}"
```

### 3.1 Variable Substitution

String values support `${VAR}` placeholders. The following are provided
automatically at runtime:

| Variable | Value |
|---|---|
| `${BITROUTER_URL}` | `http://<listen_addr>` (e.g. `http://127.0.0.1:8787`) |
| `${BITROUTER_URL_V1}` | `${BITROUTER_URL}/v1` |
| `${OPENAI_API_KEY}` | From `providers.openai.api_key` in `bitrouter.yaml` |
| `${ANTHROPIC_API_KEY}` | From `providers.anthropic.api_key` |
| `${GOOGLE_API_KEY}` | From `providers.google.api_key` |
| `${<PREFIX>_API_KEY}` | From any provider with `env_prefix` configured |

Unknown variables resolve to empty string. Env vars with empty resolved
values are **not** injected (prevents overwriting existing config with blanks).

### 3.2 Config File Patching

Config files are read, patched, and written back during `bitrouter init`.

- **JSON**: read as `serde_json::Value`, keys use dot-notation for nested
  paths (e.g. `a.b.c` sets `doc["a"]["b"]["c"]`). Missing intermediate
  objects are created automatically. Existing keys are preserved.
- **TOML**: read as `toml_edit::DocumentMut`, same dot-notation for nested
  tables. Comments and formatting in the original file are preserved.

If the file does not exist, it is created with all parent directories.

## 4. Supported Agents

### Full routing (base URL + API keys)

| Agent | Env Vars | Config Files |
|---|---|---|
| claude | `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` | — |
| codex | `OPENAI_BASE_URL`, `OPENAI_API_KEY` | `~/.codex/config.toml` |
| goose | `OPENAI_HOST`, `ANTHROPIC_HOST`, keys | — |
| cline | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` | `~/.cline/data/globalState.json` |
| openclaw | `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, keys | — |
| opencode | `LOCAL_ENDPOINT`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | — |
| deepagents | `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, keys | — |

### Partial routing (API keys only)

| Agent | Reason |
|---|---|
| gemini | No user-facing base URL env var |
| copilot | Closed system, GitHub OAuth only |
| kilo | Config file patch only (no base URL env var) |
| openhands | LLM settings via interactive `/settings` command |
| pi | Thin ACP adapter; provider config in underlying `pi` agent |
| hermes | Provider config via `hermes model` / `~/.hermes/config.yaml` |

## 5. Data Flow

### TUI spawn (runtime)

```
App::new()
  → extract_provider_keys(config.providers)
  → RoutingContext::new(listen_addr, provider_keys)

spawn_agent_provider(agent_id, config)
  → routing_ctx.resolve_env(config.routing)
  → AcpAgentProvider::new(name, config, routing_env)
      → spawn_agent_thread(name, bin, args, routing_env, handshake_tx)
          → Command::new(bin).args(args).envs(routing_env).spawn()
```

### Onboarding (`bitrouter init`)

```
run_agent_step(theme, listen_str, api_keys)
  → RoutingContext::new(listen_str, provider_keys)
  → for each discovered agent with routing.config_files:
      routing_ctx.apply_config_patches(patches)
        → read existing file (or create empty)
        → apply dot-notation key-value pairs with ${VAR} substitution
        → write back (JSON pretty-printed / TOML with preserved formatting)
```

## 6. Files

| File | Role |
|---|---|
| `bitrouter-config/src/config.rs` | `AgentRouting`, `ConfigFilePatch`, `ConfigFileFormat` structs |
| `bitrouter-config/src/agent_routing.rs` | `RoutingContext`, variable substitution, JSON/TOML patching |
| `bitrouter-config/providers/agents/*.yaml` | Per-agent routing metadata (13 agents) |
| `bitrouter-providers/src/acp/connection.rs` | `Command::envs()` injection at subprocess spawn |
| `bitrouter-providers/src/acp/provider.rs` | Threads `routing_env` through `AcpAgentProvider` |
| `bitrouter-tui/src/app/mod.rs` | Builds `RoutingContext` from `BitrouterConfig` |
| `bitrouter-tui/src/app/agent_lifecycle.rs` | Resolves per-agent env at connect time |
| `bitrouter/src/init.rs` | Config file patching during onboarding |

## 7. Future Work

- **ACP `providers/set`** (issue #300 Phase 1): When the `agent-client-protocol`
  crate adds support, implement protocol-native provider configuration as the
  primary mechanism, with env injection as fallback.
- **YAML config format**: Add `ConfigFileFormat::Yaml` for agents that use YAML
  config files (e.g. crow-cli).
- **TOML array-of-tables**: Support `[[section]]` syntax for agents like
  mistral-vibe that use TOML arrays.
- **`bitrouter agents configure`**: Standalone CLI command to re-apply config
  patches outside of `bitrouter init`.
