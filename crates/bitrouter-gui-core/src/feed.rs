use crate::protocol::*;
use futures::channel::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("feed disconnected")]
    Disconnected,
}

pub struct FeedHandle {
    pub events: mpsc::UnboundedReceiver<Event>,
    pub commands: mpsc::UnboundedSender<Command>,
}

/// A source of orchestrator events that also accepts commands. The real daemon
/// feed implements this same trait at upstream integration.
pub trait Feed {
    fn connect(self) -> FeedHandle;
}

pub struct MockFeed {
    script: Vec<Event>,
}

impl MockFeed {
    pub fn new(script: Vec<Event>) -> Self {
        Self { script }
    }

    /// The overview scenario from the spec: three agents, one pending permission.
    pub fn scenario() -> Self {
        let mk = |id: &str, harness: &str, model: &str| Session {
            id: SessionId(id.into()),
            name: id.into(),
            tab: TabId("auth-feature".into()),
            harness: harness.into(),
            model: model.into(),
            status: SessionStatus::Running,
            render_mode: RenderMode::Terminal,
        };
        Self::new(vec![
            Event::AgentSpawned {
                session: mk("auth-fix", "claude-code", "claude-opus-4"),
            },
            Event::AgentSpawned {
                session: mk("refactor-api", "qwen-code", "qwen"),
            },
            Event::AgentSpawned {
                session: mk("add-tests", "gemini-cli", "gemini-flash"),
            },
            Event::RoutingDecided {
                session: SessionId("refactor-api".into()),
                route: Route {
                    asked: "claude".into(),
                    routed: "qwen".into(),
                    rule: "cost-gate".into(),
                },
            },
            Event::RequestCompleted {
                session: SessionId("auth-fix".into()),
                model: "claude-opus-4".into(),
                prompt_tokens: 8_000,
                completion_tokens: 2_000,
                cost_micro_usd: 420_000,
                latency_ms: 1_100,
                failed_over: false,
            },
            Event::RequestCompleted {
                session: SessionId("refactor-api".into()),
                model: "qwen".into(),
                prompt_tokens: 12_000,
                completion_tokens: 3_000,
                cost_micro_usd: 1_100_000,
                latency_ms: 900,
                failed_over: false,
            },
            Event::PermissionRequested {
                session: SessionId("refactor-api".into()),
                request_id: "p1".into(),
                summary: "WRITE src/api/mod.rs".into(),
                diff: Some("- pub mod handler;\n+ pub mod users; pub mod posts;".into()),
            },
        ])
    }
}

impl Feed for MockFeed {
    fn connect(self) -> FeedHandle {
        let (ev_tx, ev_rx) = mpsc::unbounded::<Event>();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<Command>();

        for ev in self.script {
            let _ = ev_tx.unbounded_send(ev);
        }

        std::thread::spawn(move || {
            use futures::executor::block_on;
            use futures::StreamExt;
            block_on(async move {
                while let Some(cmd) = cmd_rx.next().await {
                    if let Command::SendPrompt {
                        target: Target::Session { id },
                        text,
                    } = cmd
                    {
                        let _ = ev_tx.unbounded_send(Event::SessionUpdate {
                            session: id,
                            update: SessionUpdateKind::Message {
                                text: format!("» {text}"),
                            },
                        });
                    }
                }
            });
        });

        FeedHandle {
            events: ev_rx,
            commands: cmd_tx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::StreamExt;

    #[test]
    fn mock_emits_scenario_then_echoes_prompt() -> anyhow::Result<()> {
        let mut h = MockFeed::scenario().connect();
        let first = block_on(h.events.next()).ok_or_else(|| anyhow::anyhow!("no event"))?;
        assert!(matches!(first, Event::AgentSpawned { .. }));

        h.commands.unbounded_send(Command::SendPrompt {
            target: Target::Session {
                id: SessionId("auth-fix".into()),
            },
            text: "hi".into(),
        })?;

        // drain until we see the echoed SessionUpdate
        let echoed = block_on(async {
            while let Some(ev) = h.events.next().await {
                if matches!(ev, Event::SessionUpdate { .. }) {
                    return Some(ev);
                }
            }
            None
        })
        .ok_or_else(|| anyhow::anyhow!("no echo"))?;
        assert!(matches!(echoed, Event::SessionUpdate { .. }));
        Ok(())
    }
}
