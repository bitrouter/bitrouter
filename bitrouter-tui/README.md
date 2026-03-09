# BitRouter TUI

Optional live dashboard for BitRouter, launched via `bitrouter dashboard`. Built with [Ratatui](https://ratatui.rs/) + Crossterm.

The CLI is the primary interface; the TUI is a real-time monitoring mode for local dev and debugging.

## Architecture

```
bitrouter (single binary)
├── bitrouter-api       → Warp HTTP server (serves LLM requests)
├── bitrouter-cli       → CLI commands (route, status, config)
└── bitrouter-tui       → Live dashboard (this crate, optional feature)

Shared in-process state via Arc<AppState>:
┌──────────────────────────────────────────────┐
│  AppState (defined in bitrouter-core)        │
│  ├── routing_table: RwLock<RoutingTable>     │
│  ├── metrics: DashMap<ProviderId, Stats>     │
│  ├── event_tx: broadcast::Sender<RouterEvent>│
│  └── config: RwLock<Config>                  │
└──────────────────────────────────────────────┘
         │                    │
    TUI subscribes       CLI mutates
    (broadcast rx)       (write lock)
```

The TUI runs in-process alongside the API server. It subscribes to `RouterEvent`s via `tokio::sync::broadcast` and reads shared state directly — no HTTP overhead.

## Dashboard Panels

| Panel | Description |
|---|---|
| Routing Table | Current route mappings with live provider health indicators |
| Request Stream | Real-time feed of requests (model, provider, latency, tokens) |
| Usage Metrics | Token usage aggregates, request counts, error rates per provider |
| Error Log | Stream of errors with `ProviderErrorContext` details |

## Key Dependencies

- `ratatui` — terminal UI rendering
- `crossterm` — terminal backend
- `tokio` — async runtime (shared with API server)
- `bitrouter-core` — `AppState`, `RouterEvent`, core types

## Crate Structure

```
bitrouter-tui/src/
├── lib.rs           # Public API — exposes dashboard entry point
├── app.rs           # App state and event loop
├── event.rs         # Merges terminal input + RouterEvent streams
├── ui/
│   ├── mod.rs
│   ├── dashboard.rs # Main layout (splits panels)
│   ├── routing.rs   # Routing table widget
│   ├── requests.rs  # Live request stream widget
│   ├── metrics.rs   # Usage metrics widget
│   └── errors.rs    # Error log widget
```

## Event Loop

```rust
loop {
    terminal.draw(|f| ui::render(f, &app))?;

    tokio::select! {
        key = crossterm_events.next() => handle_key(key, &mut app),
        event = router_rx.recv()     => app.apply_event(event),
    }
}
```

Two event sources merged via `tokio::select!`: terminal input (navigation, quit) and router events (request lifecycle, health changes).

## RouterEvent (defined in bitrouter-core)

```rust
pub enum RouterEvent {
    RequestStarted { id: Uuid, model: String, provider: String, timestamp: Instant },
    RequestCompleted { id: Uuid, latency: Duration, usage: LanguageModelUsage, finish_reason: LanguageModelFinishReason },
    RequestFailed { id: Uuid, error: BitrouterError },
    RouteChanged { model: String, old_target: RoutingTarget, new_target: RoutingTarget },
    ProviderHealthChanged { provider: String, healthy: bool },
}
```

## Implementation Phases

| Phase | Scope |
|---|---|
| 1 | `bitrouter-cli` — route management, status, config (prerequisite) |
| 2 | `bitrouter-core` — add `AppState`, `RouterEvent`; `bitrouter-api` emits events |
| 3 | `bitrouter-tui` — scaffold crate, basic terminal setup, routing table view |
| 4 | Request stream + metrics panels, error log |
