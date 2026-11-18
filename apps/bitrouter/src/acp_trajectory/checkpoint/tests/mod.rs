//! Single-definition checkpoint scenarios with explicit offline/live backends.
//! Offline readers run in every profile; harness builds also exercise the real
//! recorder through separate named tests using the same assertions.

mod cases;
#[cfg(feature = "db-mysql")]
mod mysql;
mod offline;

use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use crate::acp_trajectory::CanonicalStore;
use crate::acp_trajectory::fixtures::{FixtureScope, PersistedHistory};
use crate::acp_trajectory::tests::event;
use crate::storage::capture::{CaptureEvent, CaptureKind};

#[cfg(feature = "db-mysql")]
static MIGRATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy)]
enum Backend {
    Offline,
}

enum History {
    Offline(Arc<PersistedHistory>),
}

impl History {
    async fn append(&self, session: &str, event: CaptureEvent) -> Result<()> {
        match self {
            Self::Offline(history) => history.append(session, event).await,
        }
    }
}

async fn history(backend: Backend, store: &CanonicalStore, scope: FixtureScope) -> Result<History> {
    match backend {
        Backend::Offline => Ok(History::Offline(store.fixture_history(scope).await?)),
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
    }
}
