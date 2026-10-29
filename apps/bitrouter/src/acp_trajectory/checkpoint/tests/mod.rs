//! Single-definition checkpoint scenarios with explicit offline/live backends.
//! Offline readers run in every profile; harness builds also exercise the real
//! recorder through separate named tests using the same assertions.

mod cases;
#[cfg(feature = "harness")]
mod live;
#[cfg(feature = "db-mysql")]
mod mysql;
mod offline;

use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use crate::acp_trajectory::CanonicalStore;
use crate::acp_trajectory::fixtures::{FixtureScope, PersistedHistory};
use crate::acp_trajectory::tests::event;
#[cfg(feature = "harness")]
use crate::harness::acp::capture::CapturePort;
#[cfg(feature = "harness")]
use crate::harness::trajectory::{Recorder, RecordingScope};
use crate::storage::capture::{CaptureEvent, CaptureKind};

#[cfg(feature = "db-mysql")]
static MIGRATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy)]
enum Backend {
    Offline,
    #[cfg(feature = "harness")]
    Live,
}

enum History {
    Offline(Arc<PersistedHistory>),
    #[cfg(feature = "harness")]
    Live(Arc<Recorder>),
}

impl History {
    async fn append(&self, session: &str, event: CaptureEvent) -> Result<()> {
        match self {
            Self::Offline(history) => history.append(session, event).await,
            #[cfg(feature = "harness")]
            Self::Live(recorder) => {
                // The real recorder resolves session bindings from protocol
                // call state; the fixture's explicit binding is not forwarded.
                recorder.record(event).await?;
                Ok(())
            }
        }
    }
}

async fn history(backend: Backend, store: &CanonicalStore, scope: FixtureScope) -> Result<History> {
    match backend {
        Backend::Offline => Ok(History::Offline(store.fixture_history(scope).await?)),
        #[cfg(feature = "harness")]
        Backend::Live => Ok(History::Live(
            Recorder::new(
                store.db.clone(),
                RecordingScope {
                    owner: scope.owner,
                    source: scope.source,
                    controller_instance_id: scope.controller_instance_id,
                    route_scope_id: scope.route_scope_id,
                },
            )
            .await?,
        )),
    }
}

async fn new_session(history: &History, session: &str) -> Result<()> {
    history
        .append(
            session,
            event(
                CaptureKind::Request,
                Some(1),
                "session/new",
                json!({"cwd":"/workspace"}),
            ),
        )
        .await?;
    history
        .append(
            session,
            event(
                CaptureKind::Response,
                Some(1),
                "session/new",
                json!({"result":{"sessionId":session}}),
            ),
        )
        .await
}

async fn fork(
    store: &CanonicalStore,
    history: &History,
    parent: &str,
    child: &str,
    call: u64,
) -> Result<()> {
    match history {
        History::Offline(persisted) => {
            crate::acp_trajectory::tests::fork(store, persisted, parent, child, call).await
        }
        #[cfg(feature = "harness")]
        History::Live(_) => {
            history
                .append(
                    parent,
                    event(
                        CaptureKind::Request,
                        Some(call),
                        "session/fork",
                        json!({"sessionId":parent}),
                    ),
                )
                .await?;
            history
                .append(
                    child,
                    event(
                        CaptureKind::Response,
                        Some(call),
                        "session/fork",
                        json!({"result":{"sessionId":child}}),
                    ),
                )
                .await
        }
    }
}
