# BitRouter TUI

The default interactive interface for BitRouter. Running `bitrouter` launches the TUI and API server together in a single process — Claude Code-style UX.

Built with [Ratatui](https://ratatui.rs/) + Crossterm.

## UX

```
$ bitrouter                  → TUI + server (default, interactive)
$ bitrouter --headless       → server only (CI, production, systemd)
$ bitrouter status           → one-shot info, exits
```

On launch, the TUI displays the BitRouter logo and initializes the server. The terminal is fully owned by the TUI. Quitting (`q` / `Ctrl+C`) gracefully shuts down both the TUI and the server.

## Architecture

```
bitrouter (single binary, single process)
├── bitrouter-runtime   → Server lifecycle, config, control socket
├── bitrouter-api       → Warp HTTP server (serves LLM requests)
├── bitrouter-tui       → Interactive terminal UI (this crate)
└── bitrouter-core      → Shared types, traits, event bus

Startup sequence:
┌────────────────────────────────────────────────────┐
│  main()                                            │
│  ├── 1. Load config (bitrouter.toml)               │
│  ├── 2. Build AppState with broadcast channel      │
│  ├── 3. tokio::spawn(api_server)                   │
│  └── 4. tui::run(terminal, event_rx) — blocks      │
│         └── on quit: signal server shutdown         │
└────────────────────────────────────────────────────┘

Shared in-process state via Arc<AppState>:
┌──────────────────────────────────────────────────┐
│  AppState (defined in bitrouter-core)            │
│  ├── config: RwLock<BitrouterConfig>             │
│  ├── routing_table: RwLock<RoutingTable>         │
│  ├── event_tx: broadcast::Sender<RouterEvent>    │
│  └── metrics: DashMap<ProviderId, ProviderStats> │
└──────────────────────────────────────────────────┘
```

The TUI subscribes to `RouterEvent`s via `tokio::sync::broadcast` and reads shared state directly — no HTTP overhead, no IPC.

## Screens

### Splash (on launch)

```
┌──────────────────────────────────────────────┐
│                                              │
│            ██████╗ ██████╗                   │
│            ██╔══██╗██╔══██╗                  │
│            ██████╔╝██████╔╝                  │
│            ██╔══██╗██╔══██╗                  │
│            ██████╔╝██║  ██║                  │
│            ╚═════╝ ╚═╝  ╚═╝                 │
│                BitRouter                     │
│                                              │
│         Listening on 127.0.0.1:8787          │
│         3 providers configured               │
│                                              │
└──────────────────────────────────────────────┘
```

Brief splash with logo, then transitions to the dashboard.

### Dashboard (main view)

```
┌─ Routing Table ──────────────────────────────┐
│ model               provider    status       │
│ gpt-4o              openai      ● healthy    │
│ claude-sonnet-4-20250514        anthropic   ● healthy    │
│ gemini-2.0-flash    google      ○ degraded  │
├─ Request Stream ─────────────────────────────┤
│ 14:02:01  gpt-4o → openai     320ms  1.2k t │
│ 14:02:03  claude → anthropic  180ms  0.8k t │
│ 14:02:05  gemini → google     err: timeout  │
├─ Metrics ────────────────┬─ Errors ──────────┤
│ reqs: 142  err: 3 (2.1%) │ 14:02:05 google  │
│ tokens: 48.2k in / 12.1k │ Transport: conn  │
│ p50: 210ms  p99: 890ms   │ timeout after 30s│
└──────────────────────────┴───────────────────┘
  [r]outes  [s]tream  [m]etrics  [e]rrors  [q]uit
```

Four panels with keyboard navigation. Each panel can be focused/expanded.

## Key Dependencies

- `ratatui` — terminal UI rendering
- `crossterm` — terminal backend
- `tokio` — async runtime (shared with API server)
- `bitrouter-core` — `AppState`, `RouterEvent`, core types

## Crate Structure

```
bitrouter-tui/src/
├── lib.rs           # Public API: run(terminal, app_state) entry point
├── app.rs           # TUI app state, event dispatch, screen transitions
├── event.rs         # Merges terminal input + RouterEvent into unified stream
├── ui/
│   ├── mod.rs       # Top-level render dispatch
│   ├── splash.rs    # Logo + startup info
│   ├── dashboard.rs # Main layout (4-panel split)
│   ├── routing.rs   # Routing table widget
│   ├── requests.rs  # Live request stream widget
│   ├── metrics.rs   # Usage metrics widget
│   └── errors.rs    # Error log widget
```

## Event Loop

```rust
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app_state: Arc<AppState>,
) -> Result<()> {
    let mut event_rx = app_state.event_tx.subscribe();
    let mut app = App::new(app_state);

    // Splash screen
    app.set_screen(Screen::Splash);
    terminal.draw(|f| ui::render(f, &app))?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    app.set_screen(Screen::Dashboard);

    // Main loop
    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        tokio::select! {
            key = crossterm_events.next() => {
                if handle_key(key, &mut app) == Action::Quit {
                    break;
                }
            }
            event = event_rx.recv() => app.apply_event(event),
        }
    }

    Ok(())
}
```

## RouterEvent (to be added in bitrouter-core)

```rust
pub enum RouterEvent {
    RequestStarted {
        id: Uuid,
        model: String,
        provider: String,
        timestamp: Instant,
    },
    RequestCompleted {
        id: Uuid,
        latency: Duration,
        usage: LanguageModelUsage,
        finish_reason: LanguageModelFinishReason,
    },
    RequestFailed {
        id: Uuid,
        error: BitrouterError,
    },
    RouteChanged {
        model: String,
        old_target: RoutingTarget,
        new_target: RoutingTarget,
    },
    ProviderHealthChanged {
        provider: String,
        healthy: bool,
    },
}
```

## Implementation Phases

### Phase 1: Welcome screen (`bitrouter-tui`, `bitrouter`)

Get a working TUI that shows the BitRouter logo and basic info on launch. No event infrastructure, no dashboard — just the welcome screen with server running in the background. `bitrouter-core` stays untouched.

- [x] Create `bitrouter-tui/Cargo.toml` with ratatui, crossterm, tokio deps (no bitrouter-core dep yet)
- [x] Implement `lib.rs` — public `run(config: TuiConfig) -> Result<()>` entry point (owns terminal setup/teardown)
- [x] Implement `app.rs` — minimal `App` struct, running flag, key handling (just `q` / `Ctrl+C` to quit)
- [x] Implement `event.rs` — terminal input events only (crossterm `EventStream`)
- [x] Implement `ui/mod.rs` + `ui/welcome.rs` — responsive ASCII logo (large/small based on terminal width), "Open Intelligence Router for LLM Agents" tagline, server info
- [x] Add `tui` feature flag (default on) to `bitrouter/Cargo.toml`, depend on `bitrouter-tui`
- [x] Update `bitrouter/src/main.rs`:
  - Bare `bitrouter` → spawn server task + launch TUI (blocks until quit)
  - `bitrouter --headless` → server only (current `serve` behavior)
  - On TUI quit → abort server, restore terminal, exit cleanly
- [x] Pass `TuiConfig` from runtime config (listen addr, provider names) — simple struct, no Arc/shared state

### Phase 2: Event infrastructure (`bitrouter-core`, `bitrouter-api`)

Add the shared state and event bus that the dashboard will consume.

- [ ] Add `RouterEvent` enum to `bitrouter-core`
- [ ] Add `AppState` struct to `bitrouter-core` (config, routing table, event sender, metrics)
- [ ] Add `ProviderStats` struct (request count, error count, token totals, latency histogram)
- [ ] Wire `bitrouter-api` handlers to emit `RouterEvent`s on request start/complete/fail
- [ ] Update `bitrouter-runtime` to construct `AppState` and pass it through
- [ ] Update `bitrouter-tui` entry point to accept `Arc<AppState>` and subscribe to events

### Phase 3: Dashboard panels (`bitrouter-tui`)

Add the live dashboard as a second screen, navigable from the welcome screen.

- [ ] Implement `ui/dashboard.rs` — 4-panel layout frame
- [ ] `ui/routing.rs` — routing table with health indicators
- [ ] `ui/requests.rs` — scrolling request log (model, provider, latency, tokens)
- [ ] `ui/metrics.rs` — aggregate stats (request count, error rate, token totals, latency percentiles)
- [ ] `ui/errors.rs` — error stream with `ProviderErrorContext` details
- [ ] Panel focus/expand with keyboard shortcuts
- [ ] Scrolling within panels (`j`/`k` or arrow keys)

### Phase 4: Polish

- [ ] Responsive layout (adapt to terminal size)
- [ ] Color theme (provider-specific colors, health status colors)
- [ ] Help overlay (`?` key)
- [ ] Graceful degradation on small terminals
