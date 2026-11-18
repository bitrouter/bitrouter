//! Portable readback of historical ACP facts in the retained server schema.
use super::*;
use crate::storage::session_identity::SessionIdentity;
use anyhow::{Context, ensure};

#[tokio::test]
#[ignore = "requires a dedicated PostgreSQL or MySQL BITROUTER_TEST_DATABASE_URL"]
async fn retained_capture_reopens_exact_checkpoints_without_adopting_a_peer() -> Result<()> {
    let source =
        std::env::var("BITROUTER_TEST_DATABASE_URL").context("dedicated server required")?;
    let url = url::Url::parse(&source)?;
    ensure!(
        matches!(url.scheme(), "mysql" | "postgres" | "postgresql")
            && !url.path().trim_matches('/').is_empty()
    );
    let backend = if url.scheme() == "mysql" {
        sea_orm::DbBackend::MySql
    } else {
        sea_orm::DbBackend::Postgres
    };
    let fixture = crate::db::migration::server_fixture::OwnedServer::new(
        backend,
        "BITROUTER_TEST_DATABASE_URL",
    )
    .await?;
    let checked = readback(&fixture).await;
    fixture.finish(checked).await
}

async fn readback(fixture: &crate::db::migration::server_fixture::OwnedServer) -> Result<()> {
    let db = fixture.database.clone();
    crate::db::migration::m0_fixture::install(&db).await?;
    let store = CanonicalStore::new(db.clone());
    let session = uuid::Uuid::new_v4().to_string();
    let mut snapshots = Vec::new();
    for owner in ["first", "peer"] {
        let identity = SessionIdentity {
            owner: format!("{owner}-{session}"),
            source: "retained".into(),
            native_session_id: session.clone(),
        };
        let records = history(
            Backend::Offline,
            &store,
            FixtureScope {
                owner: identity.owner.clone(),
                source: identity.source.clone(),
                controller_instance_id: None,
                route_scope_id: None,
            },
        )
        .await?;
        new_session(&records, &session).await?;
        let checkpoint = store.freeze_checkpoint(&identity, 1).await?;
        let transcript = serde_json::to_value(store.transcript(&identity).await?)?;
        snapshots.push((identity, checkpoint.checkpoint_id, transcript));
    }
    ensure!(snapshots[0].1 != snapshots[1].1);
    db.close().await?;
    let db = sea_orm::Database::connect(&fixture.url).await?;
    let reopened = CanonicalStore::new(db.clone());
    for (identity, expected, transcript) in snapshots {
        let checkpoints = reopened.checkpoints(&identity).await?;
        ensure!(checkpoints.len() == 1 && checkpoints[0].checkpoint_id == expected);
        ensure!(
            reopened
                .freeze_checkpoint(&identity, 1)
                .await?
                .checkpoint_id
                == expected
        );
        ensure!(serde_json::to_value(reopened.transcript(&identity).await?)? == transcript);
    }
    db.close().await?;
    Ok(())
}
