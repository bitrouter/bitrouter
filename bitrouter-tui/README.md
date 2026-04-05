# bitrouter-tui

Terminal UI for BitRouter — manage coding agent sessions in real time.

Built on [ratatui](https://ratatui.rs) with live [Agent Client Protocol (ACP)](https://agentclientprotocol.com/) integration for communicating with coding agents over JSON-RPC/stdio.

## Features

- **Agent discovery** — auto-discovers ACP-compatible agents on PATH (native or adapter wrappers)
- **Live streaming** — renders agent responses as they stream in, including tool call status
- **Inline permissions** — approve or deny tool calls directly in the conversation panel
- **Keyboard-driven** — Tab to cycle panels, j/k to navigate, Enter to send/confirm, Esc to cancel

## Supported Agents

Agents with native ACP support work directly. Others require an adapter wrapper:

| Agent | ACP Binary | Install |
|-------|-----------|---------|
| Claude Code | `claude-agent-acp` | `npm i -g @agentclientprotocol/claude-agent-acp` |
| OpenCode | `opencode-acp` | `npm i -g opencode-acp` |
| Codex | `codex-acp` | `npm i -g codex-acp` |
| OpenClaw | `openclaw` (native) | Built-in |
| Gemini | `gemini` (native) | Built-in |
| Copilot | `copilot` (native) | Built-in |

## Architecture

```
EventHandler (mpsc channel)
  ├── terminal_event_pump (crossterm)
  └── acp_worker (dedicated thread + LocalSet)
        ├── TuiClient (impl acp::Client)
        │     ├── session_notification → AppEvent::SessionUpdate
        │     └── request_permission  → oneshot → inline Y/N prompt
        └── AgentConnection
              └── subprocess (JSON-RPC over stdio)
```

The ACP connection runs on a dedicated `std::thread` with its own single-threaded tokio runtime and `LocalSet`, because the `agent-client-protocol` crate uses `!Send` types. All communication back to the TUI event loop flows through `AppEvent` variants on the shared mpsc channel.

## Usage

The TUI launches automatically when running `bitrouter` with the `tui` feature (enabled by default):

```bash
cargo run -p bitrouter
```

Install an ACP adapter for your agent first:

```bash
npm install -g @agentclientprotocol/claude-agent-acp
```

Then type a message in the Input panel and press Enter to connect and start a session.
