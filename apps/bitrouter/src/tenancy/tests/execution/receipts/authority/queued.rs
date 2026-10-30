use super::*;
use crate::tenancy::transaction::Phase;

#[tokio::test]
async fn queued_one_and_page_recheck_revocation_before_returning_receipt_bytes() -> Result<()> {
    for page in [false, true] {
        queued_read(page, false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn queued_one_and_page_recheck_actual_credential_expiry() -> Result<()> {
    for page in [false, true] {
        queued_read(page, true).await?;
    }
    Ok(())
}

async fn queued_read(page: bool, expire: bool) -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let checked = async {
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        let _ = response.bytes().await?;
        drain(&harness.fixture).await?;
        let before = serde_json::to_vec(&harness.receipt(&client, &id).await?)?;

        let mut input = harness.fixture.issuance("queued-reader")?;
        let expires = chrono::Utc::now() + chrono::Duration::seconds(3);
        if expire {
            // Preserve this exact real UTC instant; the wire contract requires Z.
            input.expires_at = expires
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                .try_into()?;
        }
        let credential = match harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                Command::Issue(harness.fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?
        {
            StoredResponse::Issued(issued) => issued,
            _ => anyhow::bail!("reader issuance"),
        };
        let caller = harness
            .fixture
            .service
            .authenticate(credential.secret.as_deref().context("reader token")?)
            .map_err(failure)?;
        // A successful pre-read proves this exact caller was eligible before queueing.
        ensure!(
            serde_json::to_vec(&read_one(&harness.fixture, caller.clone(), &id).await?)? == before
        );

        let ordinary = harness.fixture.service.commands.capacity();
        let safety = harness.fixture.service.safety.capacity();
        let permits = harness.fixture.service.reads.available_permits();
        ensure!(ordinary > 0 && safety > 0 && permits >= 2);
        let gate = harness.fixture.service.cleanup.hold(Phase::Active);
        let service = harness.fixture.service.clone();
        let operator = harness.fixture.operator.clone();
        let tenant = harness.fixture.tenant.clone();
        let credential_id = harness.credential.credential_id.clone();
        let blocker = AbortOnDropHandle::new(tokio::spawn(async move {
            service.credential(&operator, &tenant, &credential_id).await
        }));
        tokio::time::timeout(Duration::from_secs(1), gate.entered.notified()).await?;
        let request = if page {
            read::Request::Page {
                correlation: "same-correlation".try_into()?,
                after: None,
                limit: 100,
            }
        } else {
            read::Request::One(id.clone())
        };
        let service = harness.fixture.service.clone();
        let tenant = harness.fixture.tenant.clone();
        let reader = AbortOnDropHandle::new(tokio::spawn(async move {
            service.read_receipt(caller, tenant, request).await
        }));
        tokio::time::timeout(Duration::from_secs(1), async {
            while harness.fixture.service.commands.capacity() != ordinary - 1
                || harness.fixture.service.reads.available_permits() != permits - 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("receipt read never entered the real actor queue")?;
        ensure!(!reader.is_finished() && !blocker.is_finished());

        let revoker = if expire {
            ensure!(
                chrono::Utc::now() < expires,
                "credential expired before the queued observation"
            );
            tokio::time::timeout(Duration::from_secs(4), async {
                while chrono::Utc::now() < expires {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await?;
            None
        } else {
            let service = harness.fixture.service.clone();
            let operator = harness.fixture.operator.clone();
            let command = revoke(&harness.fixture, &credential)?;
            let task = AbortOnDropHandle::new(tokio::spawn(async move {
                service.execute(operator, command).await
            }));
            tokio::time::timeout(Duration::from_secs(1), async {
                while harness.fixture.service.safety.capacity() != safety - 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .context("revoke never entered the safety queue")?;
            ensure!(!task.is_finished() && !reader.is_finished());
            Some(task)
        };
        harness.fixture.service.cleanup.release_gate();
        if let Some(revoker) = revoker {
            tokio::time::timeout(TIMEOUT, revoker)
                .await??
                .map_err(failure)?;
        }
        // The unrelated read may observe a revision change after its reply. Its
        // transaction still must finish; this is not the receipt-read assertion.
        let _ = tokio::time::timeout(TIMEOUT, blocker).await??;
        let rejected = tokio::time::timeout(TIMEOUT, reader).await??;
        ensure!(
            matches!(rejected, Err(error) if error.0.code == ErrorCode::Unauthorized),
            "queued receipt bytes escaped current credential checks"
        );
        ensure!(harness.fixture.service.current().is_ok());
        ensure!(harness.fixture.service.reads.available_permits() == permits);
        let after = harness.receipt(&client, &id).await?;
        ensure!(serde_json::to_vec(&after)? == before);
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    // Release even if queue/expiry observations failed; actor cleanup is retained.
    harness.fixture.service.cleanup.release_gate();
    finish(&harness, checked).await
}
