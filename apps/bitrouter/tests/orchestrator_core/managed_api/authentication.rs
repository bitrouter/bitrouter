//! Real virtual-key and policy changes across the managed transport boundary.

use super::*;
use bitrouter::auth::entities::api_keys;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

#[tokio::test]
async fn bound_key_policy_rejects_the_model_before_upstream_preparation() -> Result<()> {
    let fixture = configured_fixture(
        None,
        Some("id: managed-policy\nallowed_models: [different-model]\n"),
    )
    .await?;
    let (send, _tools, store, task) = harness(&fixture).await?;
    let response = post(&fixture, &fixture.key, &create("denied")).await?;
    let status = response.status();
    let response: Value = response.json().await?;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["status"], "failed", "{response}");
    assert_eq!(response["bitrouter"]["run_status"], "failed");
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("upstream")?
            .is_empty()
    );
    let records = store.lock().await;
    let state = records
        .batches
        .last()
        .context("checkpoint")?
        .decode(&Limits::default())?
        .checkpoint
        .state;
    let root = state["agent_id"].as_str().context("root")?;
    let reason = state["agents"][root]["turn"]["terminal_reason"]
        .as_str()
        .context("denial")?;
    assert!(reason.contains("fixture-model"), "{reason}");
    drop(records);
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn revoked_or_expired_keys_cannot_accept_http_or_existing_channel_work() -> Result<()> {
    for revoked in [false, true] {
        let fixture = fixture().await?;
        let (send, _tools, store, task) = harness(&fixture).await?;
        let head = store.lock().await.head.clone();
        let mut key: api_keys::ActiveModel = api_keys::Entity::find_by_id("key_owner")
            .one(&fixture.db)
            .await?
            .context("key")?
            .into();
        if revoked {
            key.active = Set(0);
        } else {
            key.expires_at = Set(Some(
                (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(),
            ));
        }
        key.update(&fixture.db).await?;
        let denied = post(&fixture, &fixture.key, &create("denied")).await?;
        assert_eq!(denied.status(), 401);
        let caps = reqwest::Client::new()
            .get(format!("{}/v1/orchestrator/capabilities", fixture.base))
            .bearer_auth(&fixture.key)
            .send()
            .await?;
        assert_eq!(caps.status(), 401);
        assert!(socket(&fixture).await.is_err());
        let mut command = envelope(
            "denied-channel",
            "input.enqueue",
            serde_json::to_value(input("denied"))?,
        );
        command["expected_state_revision"] = json!(head.state_revision);
        send.send(command).await?;
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), task).await??;
        if let Err(error) = closed {
            assert!(
                error
                    .downcast_ref::<tokio_tungstenite::tungstenite::Error>()
                    .is_some(),
                "{error}"
            );
        }
        assert_eq!(store.lock().await.head, head);
        assert!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("upstream")?
                .is_empty()
        );
        fixture.api.shutdown().await;
        drop(send);
    }
    Ok(())
}

#[tokio::test]
async fn continuation_rechecks_the_live_key_policy_without_reopening_prior_responses() -> Result<()>
{
    let fixture = configured_fixture(
        None,
        Some("id: managed-policy\nallowed_models: [fixture-model]\n"),
    )
    .await?;
    let (send, mut tools, _store, task) = harness(&fixture).await?;
    let first: Value = post(&fixture, &fixture.key, &create("first"))
        .await?
        .json()
        .await?;
    assert_eq!(first["bitrouter"]["run_status"], "waiting", "{first}");
    let command = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
        .await?
        .context("tool")?;
    let call = first["output"]
        .as_array()
        .context("output")?
        .iter()
        .find(|item| item["type"] == "function_call")
        .context("call")?;
    assert_eq!(
        first["bitrouter"]["pending_invocations"][call["call_id"].as_str().context("public ID")?]["invocation_id"],
        command.invocation_id
    );
    tokio::fs::write(
        fixture._home.path().join("policies/managed-policy.yaml"),
        "id: managed-policy\nallowed_models: [different-model]\n",
    )
    .await?;
    fixture.policy.reload().await?;
    let mut next = create("denied-continuation");
    next["previous_response_id"] = first["id"].clone();
    next["input"] = json!([{"type":"function_call_output","call_id":call["call_id"],
        "output":"file body","bitrouter":{"operation_id":"result","status":"succeeded"}}]);
    let denied: Value = post(&fixture, &fixture.key, &next).await?.json().await?;
    assert_eq!(denied["status"], "failed", "{denied}");
    assert_eq!(denied["bitrouter"]["run_status"], "failed");
    tokio::fs::write(
        fixture._home.path().join("policies/managed-policy.yaml"),
        "id: managed-policy\nallowed_models: [fixture-model]\n",
    )
    .await?;
    fixture.policy.reload().await?;
    let replay: Value = post(&fixture, &fixture.key, &next).await?.json().await?;
    assert_eq!(replay, denied);
    let replay: Value = post(&fixture, &fixture.key, &create("first"))
        .await?
        .json()
        .await?;
    assert_eq!(replay, first);
    let upstream = fixture
        .upstream
        .received_requests()
        .await
        .context("upstream")?;
    assert_eq!(
        upstream
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1
    );
    assert_eq!(
        upstream
            .iter()
            .filter(|request| request.url.path() == "/v1/responses/input_tokens")
            .count(),
        1
    );
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}
