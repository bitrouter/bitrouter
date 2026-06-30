# GUI as Real ACP Client (Minimal Single Session) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the GUI's in-process `MockFeed` with a real `AcpFeed` that drives one live ACP session through `bitrouter agent-proxy <id>`, rendering the streaming transcript and answering permission prompts.

**Architecture:** A new `AcpFeed` (in the `bitrouter-gui` app crate) owns a tokio runtime on a dedicated thread and speaks ACP as a *client* via the `agent-client-protocol` v1.0 SDK. It translates ACP `session/update` notifications into the existing `Event::SessionUpdate` variants and bridges them to the `Feed` trait's `futures` channels. `bitrouter-gui-core` gets one small, bounded change: streaming-chunk variants in `protocol` and coalescing in `reduce`. Views and the rest of core are unchanged.

**Tech Stack:** Rust, gpui, `agent-client-protocol = "1.0.0"` (+ `agent-client-protocol-schema 1.1.0`, pulled transitively), tokio, futures, uuid.

**Reference spec:** [`docs/superpowers/specs/2026-06-24-gui-acp-client-design.md`](../specs/2026-06-24-gui-acp-client-design.md)

**Ground-truth API note (verified against the pinned crate source):**
- Client entry: `agent_client_protocol::Client.builder()` with `.on_receive_notification(closure, on_receive_notification!())`, `.on_receive_request(closure, on_receive_request!())`, `.connect_with(agent, |connection: ConnectionTo<Agent>| async { ... }).await`.
- Transport: `AcpAgent::from_str("bitrouter agent-proxy claude-code")?` spawns the child (parses with `shell_words`).
- Requests: `connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?`; `NewSessionRequest::new(cwd)` → `.session_id`; `PromptRequest::new(session_id, vec![ContentBlock::Text(TextContent::new(text))])`.
- `SessionUpdate` variants: `UserMessageChunk(ContentChunk)`, `AgentMessageChunk(ContentChunk)`, `AgentThoughtChunk(ContentChunk)`, `ToolCall(ToolCall)`, `ToolCallUpdate(ToolCallUpdate)`, `Plan`, `AvailableCommandsUpdate`, `CurrentModeUpdate`, `ConfigOptionUpdate`, `SessionInfoUpdate`, `UsageUpdate`.
- `ContentChunk { content: ContentBlock, message_id: Option<MessageId>, .. }`; `MessageId(pub Arc<str>)`. **A change in `message_id` marks a new message** — this is the coalescing key.
- `ContentBlock::Text(TextContent { text: String, .. })`.
- `ToolCall { tool_call_id: ToolCallId, title: String, status: ToolCallStatus, content: Vec<ToolCallContent>, .. }`; `ToolCallId(pub Arc<str>)`; `ToolCallStatus { Pending, InProgress, Completed, Failed }`; `ToolCallContent::Diff(Diff { path: PathBuf, old_text: Option<String>, new_text: String, .. })`.
- `ToolCallUpdate { tool_call_id: ToolCallId, fields: ToolCallUpdateFields { status: Option<ToolCallStatus>, title: Option<String>, content: Option<Vec<ToolCallContent>>, .. } }`.
- Permission (agent→client request): `RequestPermissionRequest { session_id, tool_call: ToolCallUpdate, options: Vec<PermissionOption> }`; `PermissionOption { option_id: PermissionOptionId, name: String, kind: PermissionOptionKind }`; `PermissionOptionKind { AllowOnce, AllowAlways, RejectOnce, RejectAlways }`; respond with `RequestPermissionResponse::new(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)))` or `RequestPermissionOutcome::Cancelled`.

---

## File Structure

- **Modify** `crates/bitrouter-gui/Cargo.toml` — add `agent-client-protocol`, `uuid`; ensure `tokio`, `futures` present.
- **Modify** `crates/bitrouter-gui-core/src/protocol.rs` — add streaming `SessionUpdateKind` variants; add `id` to `ToolCall`; add `ToolCallUpdate`.
- **Modify** `crates/bitrouter-gui-core/src/state.rs` — `TranscriptItem` gains `message_id`/`id`; `reduce` coalesces chunks and updates tool calls.
- **Modify** `crates/bitrouter-gui/src/views/transcript.rs` — pattern-match the new `TranscriptItem` shapes (compile fix only).
- **Modify** `crates/bitrouter-gui-core/src/feed.rs` — update any `MockFeed`/test constructors that build changed variants.
- **Create** `crates/bitrouter-gui/src/acp/mod.rs` — module root.
- **Create** `crates/bitrouter-gui/src/acp/translate.rs` — pure ACP→`SessionUpdateKind` mapping + helpers (the TDD anchor).
- **Create** `crates/bitrouter-gui/src/acp/feed.rs` — `AcpFeed` implementing `Feed`.
- **Modify** `crates/bitrouter-gui/src/lib.rs` — declare `mod acp;`.
- **Modify** `crates/bitrouter-gui/src/main.rs` — select `AcpFeed` vs `MockFeed` by env.

---

## Task 0: Add the ACP dependency and confirm it resolves

**Files:**
- Modify: `crates/bitrouter-gui/Cargo.toml`

- [ ] **Step 1: Inspect current deps**

Run: `sed -n '1,40p' crates/bitrouter-gui/Cargo.toml`
Confirm whether `tokio`, `futures`, `anyhow`, `uuid` are already listed (the crate already uses `tokio::runtime::Runtime` in `ai.rs` and `futures` in `app_model.rs`, so both should exist).

- [ ] **Step 2: Add the new dependencies**

In `crates/bitrouter-gui/Cargo.toml`, under `[dependencies]`, add (and add `tokio`/`futures` only if missing):

```toml
agent-client-protocol = "1.0.0"
uuid = { version = "1", features = ["v4"] }
# Only if not already present:
# tokio = { version = "1", features = ["rt-multi-thread", "sync", "macros"] }
# futures = "0.3"
```

- [ ] **Step 3: Verify it resolves and the workspace still builds**

Run: `cargo build -p bitrouter-gui 2>&1 | tail -20`
Expected: builds successfully (the dependency resolves; `agent-client-protocol-schema 1.1.0` is pulled transitively).

- [ ] **Step 4: Commit**

```bash
git add crates/bitrouter-gui/Cargo.toml Cargo.lock
git commit -m "build(gui): add agent-client-protocol ACP client dependency"
```

---

## Task 1: Core protocol — streaming + tool-call-update variants

**Files:**
- Modify: `crates/bitrouter-gui-core/src/protocol.rs`
- Test: same file (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write a failing round-trip test for the new variants**

Add to the `tests` module in `protocol.rs`:

```rust
#[test]
fn streaming_update_kinds_round_trip() -> anyhow::Result<()> {
    let kinds = vec![
        SessionUpdateKind::MessageChunk { message_id: Some("m1".into()), text: "hel".into() },
        SessionUpdateKind::ThoughtChunk { message_id: None, text: "hmm".into() },
        SessionUpdateKind::ToolCall {
            id: "t1".into(), title: "WRITE x".into(),
            status: ToolStatus::Pending, diff: None,
        },
        SessionUpdateKind::ToolCallUpdate {
            id: "t1".into(), status: Some(ToolStatus::Ok),
            title: None, diff: Some("x\n+++ new\nv".into()),
        },
    ];
    for k in kinds {
        let back: SessionUpdateKind = serde_json::from_str(&serde_json::to_string(&k)?)?;
        assert_eq!(k, back);
    }
    Ok(())
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p bitrouter-gui-core streaming_update_kinds_round_trip 2>&1 | tail -20`
Expected: FAIL — `no variant named MessageChunk` / `ToolCall` missing field `id`.

- [ ] **Step 3: Update the `SessionUpdateKind` enum**

Replace the `SessionUpdateKind` enum in `protocol.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum SessionUpdateKind {
    /// A complete (non-streamed) assistant message — used by the mock feed.
    Message { text: String },
    /// A complete (non-streamed) thought — used by the mock feed.
    Thought { text: String },
    /// A streamed assistant-message delta. Chunks sharing a `message_id`
    /// coalesce into one transcript bubble.
    MessageChunk {
        message_id: Option<String>,
        text: String,
    },
    /// A streamed thought delta.
    ThoughtChunk {
        message_id: Option<String>,
        text: String,
    },
    /// A new tool call. `id` keys later `ToolCallUpdate`s.
    ToolCall {
        id: String,
        title: String,
        status: ToolStatus,
        diff: Option<String>,
    },
    /// An update to an existing tool call, addressed by `id`. Absent fields
    /// leave the prior value unchanged.
    ToolCallUpdate {
        id: String,
        status: Option<ToolStatus>,
        title: Option<String>,
        diff: Option<String>,
    },
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p bitrouter-gui-core streaming_update_kinds_round_trip 2>&1 | tail -20`
Expected: PASS. (The crate will still fail to *fully* compile because `state.rs` consumes the old `ToolCall` shape — that is fixed in Task 2. Running this single test compiles the test target; if the workspace test build errors on `state.rs`, proceed to Task 2 and re-run.)

- [ ] **Step 5: Commit**

```bash
git add crates/bitrouter-gui-core/src/protocol.rs
git commit -m "feat(core): add streaming chunk + tool-call-update protocol variants"
```

---

## Task 2: Core state — coalescing + tool-call update, and view compile-fix

**Files:**
- Modify: `crates/bitrouter-gui-core/src/state.rs`
- Modify: `crates/bitrouter-gui/src/views/transcript.rs`
- Test: `state.rs` tests module

- [ ] **Step 1: Write failing reduce tests**

Add to the `tests` module in `state.rs`:

```rust
#[test]
fn message_chunks_coalesce_by_message_id() -> anyhow::Result<()> {
    let mut st = State::default();
    reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
    for part in ["Hel", "lo ", "world"] {
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::MessageChunk {
                message_id: Some("m1".into()), text: part.into(),
            },
        });
    }
    let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(v.transcript.len(), 1);
    assert!(matches!(&v.transcript[0],
        TranscriptItem::Message { text, .. } if text == "Hello world"));
    Ok(())
}

#[test]
fn new_message_id_starts_new_bubble() -> anyhow::Result<()> {
    let mut st = State::default();
    reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
    for (mid, t) in [("m1", "a"), ("m2", "b")] {
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::MessageChunk {
                message_id: Some(mid.into()), text: t.into(),
            },
        });
    }
    let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(v.transcript.len(), 2);
    Ok(())
}

#[test]
fn tool_call_update_mutates_by_id() -> anyhow::Result<()> {
    let mut st = State::default();
    reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
    reduce(&mut st, Event::SessionUpdate {
        session: SessionId("s1".into()),
        update: SessionUpdateKind::ToolCall {
            id: "t1".into(), title: "WRITE x".into(),
            status: ToolStatus::Pending, diff: None,
        },
    });
    reduce(&mut st, Event::SessionUpdate {
        session: SessionId("s1".into()),
        update: SessionUpdateKind::ToolCallUpdate {
            id: "t1".into(), status: Some(ToolStatus::Ok),
            title: None, diff: Some("d".into()),
        },
    });
    let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(v.transcript.len(), 1);
    assert!(matches!(&v.transcript[0],
        TranscriptItem::ToolCall { status: ToolStatus::Ok, diff: Some(d), .. } if d == "d"));
    Ok(())
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p bitrouter-gui-core 2>&1 | tail -25`
Expected: compile errors / FAIL — `TranscriptItem::Message` has no `message_id`, no handling for the new variants.

- [ ] **Step 3: Update `TranscriptItem`**

Replace the `TranscriptItem` enum in `state.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    Message {
        /// Coalescing key from ACP `ContentChunk.message_id`; `None` for
        /// non-streamed (mock) messages.
        message_id: Option<String>,
        text: String,
    },
    Thought {
        message_id: Option<String>,
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        status: ToolStatus,
        diff: Option<String>,
    },
}
```

- [ ] **Step 4: Update the `SessionUpdate` arm of `reduce`**

In `reduce`, replace the `Event::SessionUpdate { session, update } => { ... }` arm with:

```rust
Event::SessionUpdate { session, update } => {
    if let Some(v) = state.session_mut(&session) {
        match update {
            SessionUpdateKind::Message { text } => {
                v.transcript.push(TranscriptItem::Message { message_id: None, text });
            }
            SessionUpdateKind::Thought { text } => {
                v.transcript.push(TranscriptItem::Thought { message_id: None, text });
            }
            SessionUpdateKind::MessageChunk { message_id, text } => {
                match v.transcript.last_mut() {
                    Some(TranscriptItem::Message { message_id: last, text: body })
                        if *last == message_id => body.push_str(&text),
                    _ => v.transcript.push(TranscriptItem::Message { message_id, text }),
                }
            }
            SessionUpdateKind::ThoughtChunk { message_id, text } => {
                match v.transcript.last_mut() {
                    Some(TranscriptItem::Thought { message_id: last, text: body })
                        if *last == message_id => body.push_str(&text),
                    _ => v.transcript.push(TranscriptItem::Thought { message_id, text }),
                }
            }
            SessionUpdateKind::ToolCall { id, title, status, diff } => {
                v.transcript.push(TranscriptItem::ToolCall { id, title, status, diff });
            }
            SessionUpdateKind::ToolCallUpdate { id, status, title, diff } => {
                if let Some(TranscriptItem::ToolCall {
                    title: t, status: s, diff: d, ..
                }) = v.transcript.iter_mut().rev().find(
                    |it| matches!(it, TranscriptItem::ToolCall { id: tid, .. } if *tid == id),
                ) {
                    if let Some(ns) = status { *s = ns; }
                    if let Some(nt) = title { *t = nt; }
                    if let Some(nd) = diff { *d = Some(nd); }
                }
                // Unknown id: no-op (mirrors unknown-session discipline).
            }
        }
    }
}
```

- [ ] **Step 5: Fix the transcript view to match new shapes**

Run: `grep -n "TranscriptItem::" crates/bitrouter-gui/src/views/transcript.rs`
For each `TranscriptItem::Message { text }` / `Thought { text }` pattern, add `..` so it becomes `TranscriptItem::Message { text, .. }`. For any `TranscriptItem::ToolCall { title, status, diff }` pattern, add `..` → `{ title, status, diff, .. }`. (The view renders text/status/diff; it ignores `message_id`/`id`.)

- [ ] **Step 6: Run the full core test suite + GUI build**

Run: `cargo test -p bitrouter-gui-core 2>&1 | tail -25 && cargo build -p bitrouter-gui 2>&1 | tail -15`
Expected: all core tests PASS; GUI compiles.

- [ ] **Step 7: Commit**

```bash
git add crates/bitrouter-gui-core/src/state.rs crates/bitrouter-gui/src/views/transcript.rs
git commit -m "feat(core): coalesce streamed chunks and update tool calls by id"
```

---

## Task 3: Pure ACP → event translation (the TDD anchor)

**Files:**
- Create: `crates/bitrouter-gui/src/acp/mod.rs`
- Create: `crates/bitrouter-gui/src/acp/translate.rs`
- Modify: `crates/bitrouter-gui/src/lib.rs`
- Test: `translate.rs` tests module

- [ ] **Step 1: Declare the module**

In `crates/bitrouter-gui/src/lib.rs` add (near the other `mod` declarations): `mod acp;`

Create `crates/bitrouter-gui/src/acp/mod.rs`:

```rust
//! ACP client integration: the real `Feed` over `bitrouter agent-proxy`.
pub mod translate;
pub mod feed;
```

- [ ] **Step 2: Write failing translation tests**

Create `crates/bitrouter-gui/src/acp/translate.rs` with only the tests first:

```rust
//! Pure mapping from ACP schema types to `bitrouter_gui_core` protocol types.
//! No I/O — unit-tested without spawning a process.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Diff, MessageId, PermissionOption, PermissionOptionId,
    PermissionOptionKind, RequestPermissionOutcome, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use bitrouter_gui_core::protocol::{PermissionOutcome, SessionUpdateKind, ToolStatus};

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str, mid: Option<&str>) -> ContentChunk {
        ContentChunk {
            content: ContentBlock::Text(TextContent::new(text.to_string())),
            message_id: mid.map(|m| MessageId(m.into())),
            meta: None,
        }
    }

    #[test]
    fn agent_message_chunk_maps_to_message_chunk() {
        let got = translate(SessionUpdate::AgentMessageChunk(chunk("hi", Some("m1"))));
        assert_eq!(got, Some(SessionUpdateKind::MessageChunk {
            message_id: Some("m1".into()), text: "hi".into(),
        }));
    }

    #[test]
    fn tool_call_maps_with_status_and_diff() {
        let tc = ToolCall {
            tool_call_id: ToolCallId("t1".into()),
            title: "WRITE x".into(),
            kind: Default::default(),
            status: ToolCallStatus::InProgress,
            content: vec![ToolCallContent::Diff(Diff {
                path: "x.rs".into(), old_text: Some("a".into()),
                new_text: "b".into(), meta: None,
            })],
            locations: vec![], raw_input: None, raw_output: None, meta: None,
        };
        let got = translate(SessionUpdate::ToolCall(tc));
        assert!(matches!(got, Some(SessionUpdateKind::ToolCall {
            status: ToolStatus::Running, diff: Some(_), .. })));
    }

    #[test]
    fn ignored_variants_return_none() {
        assert_eq!(translate(SessionUpdate::UserMessageChunk(chunk("u", None))), None);
    }

    #[test]
    fn status_mapping_is_total() {
        assert_eq!(map_status(ToolCallStatus::Pending), ToolStatus::Pending);
        assert_eq!(map_status(ToolCallStatus::InProgress), ToolStatus::Running);
        assert_eq!(map_status(ToolCallStatus::Completed), ToolStatus::Ok);
        assert_eq!(map_status(ToolCallStatus::Failed), ToolStatus::Failed);
    }

    fn opt(kind: PermissionOptionKind, id: &str) -> PermissionOption {
        PermissionOption { option_id: PermissionOptionId(id.into()),
            name: id.into(), kind, meta: None }
    }

    #[test]
    fn select_option_matches_kind_then_falls_back() {
        let opts = vec![
            opt(PermissionOptionKind::AllowOnce, "a1"),
            opt(PermissionOptionKind::RejectOnce, "r1"),
        ];
        match select_option(PermissionOutcome::Deny, &opts) {
            RequestPermissionOutcome::Selected(s) => assert_eq!(&*s.option_id.0, "r1"),
            _ => panic!("expected Selected"),
        }
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p bitrouter-gui acp::translate 2>&1 | tail -25`
Expected: FAIL — `translate`/`map_status`/`select_option` not found. (If a schema field name or constructor differs from the plan, the compiler will say so — fix the test literal to match the pinned crate, the source of truth, then continue.)

- [ ] **Step 4: Implement the pure functions**

Prepend to `translate.rs` (above the `#[cfg(test)]` module):

```rust
/// Map one ACP `SessionUpdate` to a core `SessionUpdateKind`. Variants the GUI
/// does not render in v1 (`UserMessageChunk`, `Plan`, `UsageUpdate`, …) → `None`.
pub fn translate(update: SessionUpdate) -> Option<SessionUpdateKind> {
    match update {
        SessionUpdate::AgentMessageChunk(c) => Some(SessionUpdateKind::MessageChunk {
            message_id: c.message_id.map(|m| m.0.to_string()),
            text: text_of(&c.content),
        }),
        SessionUpdate::AgentThoughtChunk(c) => Some(SessionUpdateKind::ThoughtChunk {
            message_id: c.message_id.map(|m| m.0.to_string()),
            text: text_of(&c.content),
        }),
        SessionUpdate::ToolCall(tc) => Some(SessionUpdateKind::ToolCall {
            id: tc.tool_call_id.0.to_string(),
            title: tc.title,
            status: map_status(tc.status),
            diff: render_diff(&tc.content),
        }),
        SessionUpdate::ToolCallUpdate(u) => Some(SessionUpdateKind::ToolCallUpdate {
            id: u.tool_call_id.0.to_string(),
            status: u.fields.status.map(map_status),
            title: u.fields.title,
            diff: u.fields.content.as_deref().and_then(render_diff),
        }),
        _ => None,
    }
}

pub fn map_status(s: ToolCallStatus) -> ToolStatus {
    match s {
        ToolCallStatus::Pending => ToolStatus::Pending,
        ToolCallStatus::InProgress => ToolStatus::Running,
        ToolCallStatus::Completed => ToolStatus::Ok,
        ToolCallStatus::Failed => ToolStatus::Failed,
    }
}

/// Choose the ACP permission option whose `kind` matches the GUI outcome,
/// falling back to the first option, then to `Cancelled` if none exist.
pub fn select_option(
    outcome: PermissionOutcome,
    options: &[PermissionOption],
) -> RequestPermissionOutcome {
    use agent_client_protocol::schema::v1::SelectedPermissionOutcome;
    let want = match outcome {
        PermissionOutcome::AllowOnce => PermissionOptionKind::AllowOnce,
        PermissionOutcome::AllowAlways => PermissionOptionKind::AllowAlways,
        PermissionOutcome::Deny => PermissionOptionKind::RejectOnce,
    };
    let chosen = options.iter().find(|o| o.kind == want).or_else(|| options.first());
    match chosen {
        Some(o) => RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(o.option_id.clone()),
        ),
        None => RequestPermissionOutcome::Cancelled,
    }
}

fn text_of(b: &ContentBlock) -> String {
    match b {
        ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

fn render_diff(content: &[ToolCallContent]) -> Option<String> {
    content.iter().find_map(|c| match c {
        ToolCallContent::Diff(d) => {
            let old = d.old_text.clone().unwrap_or_default();
            Some(format!("{}\n--- old\n{}\n+++ new\n{}", d.path.display(), old, d.new_text))
        }
        _ => None,
    })
}
```

> If a constructor or field differs from the pinned crate (e.g. `PermissionOption` requires a different field set), trust the compiler error over this plan and adjust — the crate at `~/.cargo/registry/src/.../agent-client-protocol-schema-1.1.0` is authoritative. Note `ToolCallId`/`MessageId`/`PermissionOptionId` are `(pub Arc<str>)` newtypes, so `.0` derefs to `str`.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p bitrouter-gui acp::translate 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/bitrouter-gui/src/acp/mod.rs crates/bitrouter-gui/src/acp/translate.rs crates/bitrouter-gui/src/lib.rs
git commit -m "feat(gui): pure ACP->event translation layer with tests"
```

---

## Task 4: `AcpFeed` — the real feed over `bitrouter agent-proxy`

**Files:**
- Create: `crates/bitrouter-gui/src/acp/feed.rs`

> This is the only task with real concurrency. It has one genuine design hazard called out below — resolve it with the spike in Step 1 before writing the full feed.

- [ ] **Step 1: Spike the interactive driving model (throwaway, do NOT commit)**

The deadlock hazard: if the command loop `.await`s a prompt to completion, and the agent issues a `session/request_permission` *during* that turn, the loop cannot process the resolving `ResolvePending` command → deadlock. The feed must keep the command loop responsive while a prompt is in flight.

Write a temporary `examples/acp_spike.rs` in the GUI crate that:
1. builds `agent_client_protocol::Client.builder()` with a notification handler that `eprintln!`s each `notification.update`,
2. a permission handler that auto-selects the first option,
3. `.connect_with(AcpAgent::from_str("bitrouter agent-proxy claude-code")?, |conn| async { initialize → new_session → send one prompt → print stop_reason })`.

Run: `BITROUTER_GUI_AGENT=claude-code cargo run -p bitrouter-gui --example acp_spike 2>&1 | tail -40`
Confirm: (a) the `initialize` handshake **succeeds** against bitrouter's hand-rolled proxy (this is the protocol-version-skew check from the spec §9), and (b) you observe streamed `AgentMessageChunk` updates and a final stop reason.

Decide the driving model from what compiles:
- **Primary:** spawn each prompt as its own task so the command loop stays free (Step 3 code below).
- **Fallback:** if the SDK exposes an `ActiveSession` with `read_update()` (it does — `ActiveSession::read_update -> SessionMessage`), drive updates by polling `read_update` in a task and send prompts via the active session; the command loop still never blocks on a turn.

Delete `examples/acp_spike.rs` after the spike. Record which model worked in the commit message of Step 5.

- [ ] **Step 2: Write the `AcpFeed` skeleton + a construction test**

Create `crates/bitrouter-gui/src/acp/feed.rs`:

```rust
//! `AcpFeed` — a real [`Feed`] that drives one ACP session through
//! `bitrouter agent-proxy <id>`. Owns a tokio runtime on a dedicated thread
//! (the `ai.rs` pattern) and bridges ACP to the `Feed`'s `futures` channels.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};
use bitrouter_gui_core::feed::{Feed, FeedHandle};
use bitrouter_gui_core::protocol::{
    Command, Event, PermissionOutcome, RenderMode, Session, SessionId, SessionStatus, TabId,
    Target,
};
use futures::channel::mpsc;
use futures::StreamExt;

use super::translate::{select_option, translate};

/// Outstanding permission requests, keyed by the GUI-facing request id.
type Pending = Arc<Mutex<HashMap<String, futures::channel::oneshot::Sender<PermissionOutcome>>>>;

/// A real feed bound to one agent command line (e.g. `bitrouter agent-proxy claude-code`).
pub struct AcpFeed {
    agent_command: String,
    agent_id: String,
}

impl AcpFeed {
    /// Build a feed. `agent_id` is the bitrouter agent name; `bin` is the
    /// resolved `bitrouter` binary (PATH name or absolute path).
    pub fn new(bin: &str, agent_id: &str) -> Self {
        Self {
            agent_command: format!("{bin} agent-proxy {agent_id}"),
            agent_id: agent_id.to_string(),
        }
    }

    /// Resolve config from the environment: `BITROUTER_BIN` (default `bitrouter`)
    /// and `BITROUTER_GUI_AGENT` (default `claude-code`).
    pub fn from_env() -> Self {
        let bin = std::env::var("BITROUTER_BIN").unwrap_or_else(|_| "bitrouter".into());
        let agent = std::env::var("BITROUTER_GUI_AGENT").unwrap_or_else(|_| "claude-code".into());
        Self::new(&bin, &agent)
    }
}

impl Feed for AcpFeed {
    fn connect(self) -> FeedHandle {
        let (event_tx, event_rx) = mpsc::unbounded::<Event>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();

        let agent_command = self.agent_command;
        let agent_id = self.agent_id;
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(run(agent_command, agent_id, event_tx, cmd_rx));
        });

        FeedHandle { events: Box::pin(event_rx), commands: cmd_tx }
    }
}
```

Add a test at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_builds_proxy_command() {
        // Default agent + bin when env is unset is the documented convention.
        let feed = AcpFeed::new("bitrouter", "claude-code");
        assert_eq!(feed.agent_command, "bitrouter agent-proxy claude-code");
    }
}
```

- [ ] **Step 3: Implement `run` (the tokio-side driver)**

Append to `feed.rs`. This uses the **primary** (spawn-the-prompt) model from the spike; if the spike chose the `ActiveSession` fallback, adapt the prompt-send accordingly but keep the same channel wiring and the same "loop never blocks on a turn" property.

```rust
/// Drive one ACP session to completion. Sends `AgentSpawned` on success and
/// `AgentExited` when the session ends or fails.
async fn run(
    agent_command: String,
    agent_id: String,
    event_tx: mpsc::UnboundedSender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    let session_id_str = "acp-session".to_string();
    let result = drive(
        &agent_command, &agent_id, &session_id_str,
        event_tx.clone(), &pending, &mut cmd_rx,
    )
    .await;

    let code = if result.is_ok() { 0 } else { 1 };
    if let Err(e) = &result {
        // Surface the failure in the transcript so the UI shows a dead session.
        let _ = event_tx.unbounded_send(Event::SessionUpdate {
            session: SessionId(session_id_str.clone()),
            update: bitrouter_gui_core::protocol::SessionUpdateKind::Message {
                text: format!("ACP session ended: {e}"),
            },
        });
    }
    let _ = event_tx.unbounded_send(Event::AgentExited {
        session: SessionId(session_id_str),
        code,
    });
}

async fn drive(
    agent_command: &str,
    agent_id: &str,
    session_id_str: &str,
    event_tx: mpsc::UnboundedSender<Event>,
    pending: &Pending,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
) -> anyhow::Result<()> {
    let agent = AcpAgent::from_str(agent_command)
        .map_err(|e| anyhow::anyhow!("spawn agent-proxy: {e:?}"))?;

    let notif_tx = event_tx.clone();
    let session_for_notif = session_id_str.to_string();
    let perm_tx = event_tx.clone();
    let perm_pending = pending.clone();
    let session_for_perm = session_id_str.to_string();
    let agent_id = agent_id.to_string();
    let session_id_str = session_id_str.to_string();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let tx = notif_tx.clone();
                let sid = session_for_notif.clone();
                async move {
                    if let Some(update) = translate(notification.update) {
                        let _ = tx.unbounded_send(Event::SessionUpdate {
                            session: SessionId(sid), update,
                        });
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            move |request: RequestPermissionRequest, responder, _conn| {
                let tx = perm_tx.clone();
                let pending = perm_pending.clone();
                let sid = session_for_perm.clone();
                async move {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    let summary = request.tool_call.fields.title.clone()
                        .unwrap_or_else(|| "permission requested".into());
                    let diff = request.tool_call.fields.content.as_deref()
                        .and_then(super::translate_render_diff);
                    let (otx, orx) = futures::channel::oneshot::channel::<PermissionOutcome>();
                    pending.lock().unwrap().insert(request_id.clone(), otx);
                    let _ = tx.unbounded_send(Event::PermissionRequested {
                        session: SessionId(sid), request_id, summary, diff,
                    });
                    // Park until the GUI resolves; the command loop fires `otx`.
                    let outcome = orx.await.unwrap_or(PermissionOutcome::Deny);
                    responder.respond(RequestPermissionResponse::new(
                        select_option(outcome, &request.options),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let new_session = connection
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_else(|_| "/".into()),
                ))
                .block_task()
                .await?;
            let acp_session_id = new_session.session_id;

            let _ = event_tx.unbounded_send(Event::AgentSpawned {
                session: Session {
                    id: SessionId(session_id_str.clone()),
                    name: agent_id.clone(),
                    tab: TabId("main".into()),
                    harness: agent_id.clone(),
                    model: String::new(),
                    status: SessionStatus::Running,
                    render_mode: RenderMode::Acp,
                },
            });

            // Command loop: never blocks on a prompt turn (spawn prompts), so a
            // mid-turn permission request can still be resolved.
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
                    Command::SendPrompt { target: Target::Session { id }, text }
                        if id.0 == session_id_str =>
                    {
                        let req = PromptRequest::new(
                            acp_session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(text))],
                        );
                        let sent = connection.send_request(req);
                        // Drive the turn in the background; updates stream via the
                        // notification handler.
                        tokio::spawn(async move { let _ = sent.block_task().await; });
                    }
                    Command::ResolvePending { request_id: Some(rid), outcome, .. } => {
                        if let Some(tx) = pending.lock().unwrap().remove(&rid) {
                            let _ = tx.send(outcome);
                        }
                    }
                    Command::StopAgent { .. } => break,
                    _ => {}
                }
            }
            Ok(())
        })
        .await?;

    Ok(())
}
```

> Three things to confirm against the compiler in this step, all isolated to this file:
> 1. **Permission diff helper** — `super::translate_render_diff` is referenced for convenience. Either expose `render_diff` from `translate.rs` as `pub fn render_diff(...)` and call `super::translate::render_diff`, or inline the diff extraction. Pick one; keep `translate.rs`'s tests green.
> 2. **`connection` capture in `tokio::spawn`** — if `ConnectionTo<Agent>` is not `Clone`/`'static`-shareable, the spike's `ActiveSession` model is the fallback: hold the `ActiveSession` and call its prompt method instead of `connection.send_request` inside a spawn. The channel wiring is identical.
> 3. **`pending` mutex across `.await`** — uses `std::sync::Mutex` and only locks for the insert/remove (never held across `.await`), which is correct. Do not hold the guard across an await point.

- [ ] **Step 4: Build and run unit tests**

Run: `cargo test -p bitrouter-gui acp:: 2>&1 | tail -25 && cargo build -p bitrouter-gui 2>&1 | tail -15`
Expected: `translate` tests + `from_env_builds_proxy_command` PASS; crate compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/bitrouter-gui/src/acp/feed.rs
git commit -m "feat(gui): AcpFeed drives one ACP session over bitrouter agent-proxy

Driving model used: <primary spawn-prompt | ActiveSession fallback>."
```

---

## Task 5: Wire `AcpFeed` into `main.rs`

**Files:**
- Modify: `crates/bitrouter-gui/src/main.rs`

- [ ] **Step 1: Inspect how the feed is constructed today**

Run: `grep -n "MockFeed\|AppModel::new\|Feed" crates/bitrouter-gui/src/main.rs`
Identify the single construction site that passes a feed into `AppModel::new`.

- [ ] **Step 2: Select the feed by env**

At that construction site, choose the real feed when `BITROUTER_GUI_AGENT` is set (or always attempt it, falling back to mock only when explicitly requested). Concretely:

```rust
// Real ACP feed unless BITROUTER_GUI_MOCK=1 forces the scripted mock.
if std::env::var("BITROUTER_GUI_MOCK").is_ok() {
    cx.new(|cx| AppModel::new(bitrouter_gui_core::feed::MockFeed::scenario(), cx))
} else {
    cx.new(|cx| AppModel::new(crate::acp::feed::AcpFeed::from_env(), cx))
}
```

Adapt the exact `cx`/closure form to the existing code. Ensure `AcpFeed` is reachable (it is `pub` under `mod acp`; if `main.rs` is a separate binary that uses the lib crate, reference it as `bitrouter_gui::acp::feed::AcpFeed` and make `mod acp;` → `pub mod acp;` in `lib.rs`).

- [ ] **Step 3: Build**

Run: `cargo build -p bitrouter-gui 2>&1 | tail -15`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/bitrouter-gui/src/main.rs crates/bitrouter-gui/src/lib.rs
git commit -m "feat(gui): select AcpFeed by default, MockFeed via BITROUTER_GUI_MOCK"
```

---

## Task 6: End-to-end manual verification against real BitRouter

**Files:** none (verification only)

- [ ] **Step 1: Confirm the agent is configured**

Run: `grep -A4 "agents:" /Users/kelsen/Documents/Code/bitrouter/examples/bitrouter.yaml` (or the user's active `bitrouter.yaml`).
Confirm a `claude-code` agent exists under `agents:`. If the config lives elsewhere, ensure `bitrouter agent-proxy claude-code` can find it (run the GUI from that directory, since `agent-proxy` loads config from the working directory / standard path).

- [ ] **Step 2: Sanity-check the proxy outside the GUI**

Run: `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' | bitrouter agent-proxy claude-code 2>&1 | head -5`
Expected: a JSON-RPC `result` line with a protocol version + agent capabilities (proves the proxy and `initialize` work, independent of the SDK). If this errors, the issue is upstream config, not the GUI.

- [ ] **Step 3: Run the GUI against the real agent**

Run: `BITROUTER_GUI_AGENT=claude-code cargo run -p bitrouter-gui 2>&1 | tee /tmp/acp-gui.log`
Then in the GUI: confirm a session appears (`AgentSpawned`), send a prompt, and watch the transcript **stream** token-by-token. Trigger a file-write tool call and confirm the permission modal appears; approve it and confirm the agent proceeds.

- [ ] **Step 4: Record the protocol-skew finding**

In the session notes (or the spec's §9), record whether the rust-sdk v1.0 `initialize` interoperated cleanly with bitrouter's hand-rolled proxy. This is the empirical input that decides whether the deferred upstream-SDK migration becomes urgent.

- [ ] **Step 5: Final commit (docs/notes only, if any)**

```bash
git add -A && git commit -m "docs: record ACP end-to-end verification + protocol-skew finding" || true
```

---

## Self-Review (completed by plan author)

- **Spec coverage:** §4 decisions → Tasks 0/3/4/5; §6 core change → Tasks 1–2; §7 AcpFeed → Task 4; §8 pure translation → Task 3; §10 testing → Tasks 1–4 unit tests + Task 6 manual; §9 protocol-skew verification → Task 4 Step 1 + Task 6 Step 4; §11 wiring → Task 5. Cost HUD is an explicit non-goal — no task, by design (note: ACP *does* carry a `UsageUpdate` variant, a natural future hook, recorded for the deferred cost work).
- **Placeholder scan:** none — every code step carries complete code; the two genuinely SDK-uncertain points (interactive driving model, `connection` shareability) are handled by an explicit spike (Task 4 Step 1) with a concrete primary + named fallback, not a hand-wave.
- **Type consistency:** `SessionUpdateKind` variants (Task 1) match their `reduce` arms (Task 2), the `translate` outputs (Task 3), and the `AcpFeed` event sends (Task 4). `TranscriptItem` shapes (Task 2) match the view fix (Task 2 Step 5). `PermissionOutcome`/`PermissionOptionKind` mapping is consistent between `select_option` (Task 3) and the permission handler (Task 4).
