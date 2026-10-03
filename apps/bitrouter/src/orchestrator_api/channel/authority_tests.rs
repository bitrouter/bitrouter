//! Exercise the final delivery check with actual persisted virtual-key changes.

use super::*;
use crate::auth::{db, entities::api_keys, keys};
use sea_orm::{ActiveModelTrait, EntityTrait, Set, TransactionTrait};

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn principal() -> Result<(Principal, sea_orm::DatabaseConnection), Box<dyn std::error::Error>>
{
    let connection = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&connection).await?;
    db::upsert_user(&connection, "owner").await?;
    let key = keys::generate();
    db::insert_api_key(
        &connection,
        &db::NewApiKey {
            id: "dispatch-key".into(),
            key_hash: key.hash,
            user_id: "owner".into(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", key.secret).parse()?,
    );
    let principal = Principal::authenticate(&connection, &headers)
        .await
        .map_err(|error| error.0)?;
    Ok((principal, connection))
}

async fn invalidate(connection: &sea_orm::DatabaseConnection, expire: bool) -> TestResult {
    let mut key: api_keys::ActiveModel = api_keys::Entity::find_by_id("dispatch-key")
        .one(connection)
        .await?
        .ok_or("key")?
        .into();
    if expire {
        key.expires_at = Set(Some(
            (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(),
        ));
    } else {
        key.active = Set(0);
    }
    key.update(connection).await?;
    Ok(())
}

fn port(principal: Principal) -> Result<RemotePort, CoreError> {
    let limits = Limits::default();
    Ok(RemotePort::new(
        principal,
        bitrouter_orchestrator::core::protocol::OwnershipGrant {
            session_id: "session".into(),
            harness_id: "harness".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        },
        limits.clone(),
        Arc::new(output::Budget::default()).scope(limits.ephemeral_bytes, None)?,
        Arc::new(output::Budget::default()).scope(limits.unacknowledged_bytes, None)?,
    ))
}

#[tokio::test]
async fn queued_control_frame_rechecks_revocation_and_expiry_before_socket_write() -> TestResult {
    for expire in [false, true] {
        let (principal, db) = principal().await?;
        let port = Arc::new(port(principal.clone())?);
        let (sender, outgoing) = mpsc::channel(1);
        let connection = port.attach(sender).await?;
        let sending = tokio::spawn({
            let port = port.clone();
            let connection = connection.clone();
            async move {
                port.transmit(&connection, ServerMessage::Head(Default::default()))
                    .await
            }
        });
        // A queued frame proves transmit passed its original authentication
        // and byte admission. Withhold the writer until that key is invalid.
        tokio::time::timeout(Duration::from_secs(60), async {
            while outgoing.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        invalidate(&db, expire).await?;
        let (sink, mut observed) = futures::channel::mpsc::unbounded();
        let writer = BufferedHalf {
            half: sink,
            owner: connection.buffered.clone(),
        };
        tokio::time::timeout(
            Duration::from_secs(60),
            write_messages(writer, outgoing, principal, connection.closed.clone()),
        )
        .await?;
        let rejected = sending.await?.err().ok_or("expected denial")?;
        // transmit may observe the channel fence before the writer receipt.
        assert!(matches!(
            rejected.code,
            ErrorCode::UnauthorizedScope | ErrorCode::CheckpointUnavailable
        ));
        assert!(observed.next().await.is_none());
        assert!(connection.closed.is_cancelled());
        assert_eq!(port.control.reserve(64, true).await?.len(), 64);
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn dispatch_authorization_fences_a_live_channel_when_its_key_is_revoked() -> TestResult {
    let (principal, db) = principal().await?;
    let port = port(principal)?;
    let (sender, _outgoing) = mpsc::channel(1);
    let connection = port.attach(sender).await?;
    port.authorize_dispatch().await?;
    invalidate(&db, false).await?;
    assert_eq!(
        port.authorize_dispatch().await.err().ok_or("denied")?.code,
        ErrorCode::UnauthorizedScope
    );
    assert!(connection.closed.is_cancelled());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn disconnect_interrupts_delivery_while_authority_database_is_busy() -> TestResult {
    let (principal, db) = principal().await?;
    let port = port(principal.clone())?;
    let (sender, outgoing) = mpsc::channel(1);
    let connection = port.attach(sender.clone()).await?;
    let (sent, delivered) = oneshot::channel();
    sender
        .send(Outgoing {
            text: "queued".into(),
            sent,
        })
        .await?;
    let (sink, mut observed) = futures::channel::mpsc::unbounded();
    // The in-memory fixture pool has one connection. Hold it to force the
    // writer's revalidation to wait, independently of socket readiness.
    let transaction = db.begin().await?;
    let mut writing = Box::pin(write_messages(
        BufferedHalf {
            half: sink,
            owner: connection.buffered.clone(),
        },
        outgoing,
        principal,
        connection.closed.clone(),
    ));
    assert!(futures::poll!(writing.as_mut()).is_pending());
    connection.closed.cancel();
    tokio::time::timeout(Duration::from_secs(5), writing).await?;
    assert_eq!(
        delivered.await?.err().ok_or("denied")?.code,
        ErrorCode::CheckpointUnavailable
    );
    assert!(observed.next().await.is_none());
    transaction.rollback().await?;
    db.close().await?;
    Ok(())
}
