//! Incremental actual H1/h2 bodies, receipt commit barriers and034 compensation.
use super::{
    fixture::{Harness, Protocol, Release, client, completed, identity, modes},
    *,
};
use crate::continuation::tenant::state::Ticket;
use crate::tenancy::transaction::{Disposition, Phase};

async fn install_publication_audit(harness: &Harness) -> Result<()> {
    // SQLite executes these AFTER triggers in the writer's transaction. Audit
    // rows therefore disappear with a failed COMMIT or acknowledged rollback.
    // https://www.sqlite.org/lang_createtrigger.html
    harness.fixture.db.execute_unprepared(
        "CREATE TABLE receipt035_publication_audit (seq INTEGER PRIMARY KEY, event TEXT NOT NULL, publication_state TEXT NOT NULL, continuation_identity TEXT NOT NULL, publication_generation TEXT NOT NULL, publication_instance_id TEXT NOT NULL, installation_id TEXT NOT NULL, tenant_id TEXT NOT NULL, creator_principal_id TEXT NOT NULL, owner_identity TEXT NOT NULL)"
    ).await?;
    // Names and aliases are fixed test-authored identifiers. Values come from
    // the actual OLD/NEW row, not an expected ticket copied into the audit log.
    let insert = |event: &str, image: &str| {
        format!(
            "INSERT INTO receipt035_publication_audit (event, publication_state, continuation_identity, publication_generation, publication_instance_id, installation_id, tenant_id, creator_principal_id, owner_identity) VALUES ('{event}', {image}.publication_state, {image}.continuation_identity, {image}.publication_generation, {image}.publication_instance_id, {image}.installation_id, {image}.tenant_id, {image}.creator_principal_id, {image}.owner_identity);"
        )
    };
    for (name, operation, body) in [
        (
            "receipt035_publication_insert",
            "INSERT",
            insert("insert", "NEW"),
        ),
        (
            "receipt035_publication_update",
            "UPDATE",
            format!(
                "{} {}",
                insert("update_old", "OLD"),
                insert("update_new", "NEW")
            ),
        ),
        (
            "receipt035_publication_delete",
            "DELETE",
            insert("delete", "OLD"),
        ),
    ] {
        harness
            .fixture
            .db
            .execute_unprepared(&format!(
                "CREATE TRIGGER {name} AFTER {operation} ON provider_continuations BEGIN {body} END"
            ))
            .await?;
    }
    Ok(())
}

async fn publication_ticket(harness: &Harness) -> Result<Ticket> {
    // Entry reserved this actual ticket before the authenticated provider POST.
    // Observe it while the provider is held; never construct a test ticket/proof.
    let mut tickets = harness
        .fixture
        .service
        .http_continuation
        .state
        .pending_tickets()
        .map_err(|_| anyhow::anyhow!("HTTP publication state"))?;
    ensure!(
        tickets.len() == 1,
        "request did not reserve exactly one HTTP publication"
    );
    let ticket = tickets.pop().context("actual HTTP publication ticket")?;
    ensure!(ticket.owner.installation() == &harness.fixture.installation);
    ensure!(ticket.owner.tenant() == &harness.fixture.tenant);
    ensure!(ticket.owner.principal() == &harness.fixture.principal);
    let route = ticket.owner.route();
    ensure!(route.resource_id.as_str() == MODEL);
    ensure!(route.owner_id.as_str() == harness.fixture.tenant.as_str());
    ensure!(route.owner_kind == bitrouter_ai::tenancy::authority::OwnerKind::Tenant);
    ensure!(route.resource_kind == bitrouter_ai::tenancy::authority::ResourceKind::Route);
    ensure!(
        ticket.public_id
            == crate::gateway::model::protocol::responses::encode_gateway_continuation_id(
                ticket.execution.as_str()
            )?
    );
    let crypto = harness
        .fixture
        .service
        .http_continuation
        .state
        .crypto()
        .map_err(|_| anyhow::anyhow!("acknowledged HTTP crypto"))?;
    ensure!(ticket.physical == crypto.identity(&ticket.owner, &ticket.public_id)?);
    ensure!(!ticket.generation.is_empty() && !ticket.producer.is_empty());
    ensure!(harness.count("provider_continuations").await? == 0);
    ensure!(harness.count("receipt035_publication_audit").await? == 0);
    Ok(ticket)
}

async fn publication_compensated(harness: &Harness, ticket: &Ticket) -> Result<()> {
    let expected = [
        ("insert", "provisional"),
        ("update_old", "provisional"),
        ("update_new", "delivering"),
        ("update_old", "delivering"),
        ("update_new", "active"),
        ("delete", "active"),
    ];
    // Inspect all audit namespaces, not a filtered subset that could conceal a
    // wrong owner/generation. The seventh row makes extra writes fail boundedly.
    let rows = harness.fixture.db.query_all(Statement::from_string(
        harness.fixture.db.get_database_backend(),
        "SELECT seq, event, publication_state, continuation_identity, publication_generation, publication_instance_id, installation_id, tenant_id, creator_principal_id, owner_identity FROM receipt035_publication_audit ORDER BY seq LIMIT 7",
    )).await?;
    ensure!(
        rows.len() == expected.len(),
        "missing publication/compensation or extra writes: {} audit rows",
        rows.len()
    );
    let owner = harness
        .fixture
        .service
        .http_continuation
        .state
        .crypto()
        .map_err(|_| anyhow::anyhow!("acknowledged HTTP crypto"))?
        .owner_tag(&ticket.owner)?;
    for (index, (row, (event, state))) in rows.iter().zip(expected).enumerate() {
        ensure!(row.try_get::<i64>("", "seq")? == i64::try_from(index)? + 1);
        for (column, expected) in [
            ("event", event),
            ("publication_state", state),
            ("continuation_identity", ticket.physical.as_str()),
            ("publication_generation", ticket.generation.as_str()),
            ("publication_instance_id", ticket.producer.as_str()),
            ("installation_id", ticket.owner.installation().as_str()),
            ("tenant_id", ticket.owner.tenant().as_str()),
            ("creator_principal_id", ticket.owner.principal().as_str()),
            ("owner_identity", owner.as_str()),
        ] {
            ensure!(
                row.try_get::<String>("", column)? == expected,
                "publication audit row {index} mismatched {column}"
            );
        }
    }
    Ok(())
}

// These observations distinguish provider facts from the caller-poll ACK.
// Match only complete fixture SSE frames; a partial JSON/token is not a frame.
#[derive(Default)]
struct WireEvidence {
    provisional_finish: bool,
    provisional_usage: bool,
    final_frames: usize,
    successful_final: bool,
    failed: bool,
    error_closed: bool,
}
fn evidence(bytes: &[u8]) -> WireEvidence {
    let mut observed = WireEvidence::default();
    let text = String::from_utf8_lossy(bytes);
    for frame in text
        .split_inclusive("\n\n")
        .filter(|frame| frame.ends_with("\n\n"))
    {
        for data in frame.lines().filter_map(|line| line.strip_prefix("data: ")) {
            if data == "[DONE]" {
                observed.final_frames += 1;
                if observed.failed {
                    observed.error_closed = true;
                } else {
                    observed.successful_final = true;
                }
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if value.get("error").is_some_and(|value| !value.is_null())
                || value["type"] == "response.failed"
            {
                observed.failed = true;
            }
            if value["type"] == "response.completed" || value["type"] == "response.incomplete" {
                observed.final_frames += 1;
                observed.successful_final = true;
            }
            observed.provisional_finish |= value["choices"].as_array().is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("finish_reason")
                        .is_some_and(|reason| !reason.is_null())
                })
            });
            observed.provisional_usage |= value["usage"]["prompt_tokens"].as_u64() == Some(11)
                && value["usage"]["completion_tokens"].as_u64() == Some(7);
        }
    }
    observed
}
fn has_success(bytes: &[u8]) -> bool {
    evidence(bytes).successful_final
}
fn has_error(bytes: &[u8]) -> bool {
    evidence(bytes).failed
}
async fn provisional(response: &mut reqwest::Response, seen: &mut Vec<u8>) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        loop {
            let observed = evidence(seen);
            ensure!(
                observed.final_frames == 0 && !observed.failed,
                "final signal escaped the held receipt transaction"
            );
            if observed.provisional_finish && observed.provisional_usage {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(
                chunk(response, seen).await?,
                "missing provisional finish/usage before receipt disposition"
            );
        }
    })
    .await??;
    Ok(())
}
async fn chunk(response: &mut reqwest::Response, seen: &mut Vec<u8>) -> Result<bool> {
    let Some(bytes) = response.chunk().await? else {
        return Ok(false);
    };
    let size = seen
        .len()
        .checked_add(bytes.len())
        .context("wire length overflow")?;
    ensure!(size <= WIRE_LIMIT, "wire inspection exceeded bound");
    seen.extend_from_slice(&bytes);
    ensure!(
        !String::from_utf8_lossy(seen).contains("resp-fault-private"),
        "private response identity escaped"
    );
    Ok(true)
}
async fn prefix(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut seen = Vec::new();
    tokio::time::timeout(WAIT, async {
        loop {
            ensure!(
                chunk(response, &mut seen).await?,
                "wire ended before nonterminal output"
            );
            ensure!(!has_success(&seen), "terminal escaped gated provider");
            if String::from_utf8_lossy(&seen).contains("owned reply") {
                break Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    Ok(seen)
}
async fn finish(response: &mut reqwest::Response, seen: &mut Vec<u8>) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        while chunk(response, seen).await? {}
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
async fn held_terminal(response: &mut reqwest::Response, seen: &mut Vec<u8>) -> Result<()> {
    provisional(response, seen).await?;
    let check = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if !chunk(response, seen).await? {
                return Err::<(), anyhow::Error>(anyhow::anyhow!(
                    "wire closed before receipt disposition"
                ));
            }
            ensure!(
                evidence(seen).final_frames == 0 && !has_error(seen),
                "final acknowledgement escaped the held receipt transaction"
            );
        }
    })
    .await;
    match check {
        Err(_) => Ok(()),
        Ok(result) => {
            result?;
            anyhow::bail!("held receipt unexpectedly finished wire")
        }
    }
}

#[tokio::test]
async fn incremental_final_ack_waits_for_actual_receipt_commit_h1_h2() -> Result<()> {
    for &h2 in modes() {
        let mut harness = Harness::new(Protocol::Chat, None).await?;
        let http = client(h2)?;
        let request = harness.request(&http, true);
        let sending = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
        harness.provider.accepted().await?;
        harness.provider.open.cancel();
        let mut response = tokio::time::timeout(WAIT, sending).await???;
        ensure!(response.status() == StatusCode::OK);
        let id = identity(&response, h2)?;
        let mut seen = prefix(&mut response).await?;
        let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
        let release = Release(harness.fixture.service.clone());
        harness.provider.tail.cancel();
        tokio::time::timeout(WAIT, gate.entered.notified()).await?;
        ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(
            harness
                .fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none()
        );
        let held = held_terminal(&mut response, &mut seen).await;
        drop(release);
        held?;
        finish(&mut response, &mut seen).await?;
        ensure!(
            has_success(&seen) && !has_error(&seen),
            "confirmed receipt did not release a successful terminal"
        );
        let receipt = harness.receipt(&id).await?;
        completed(&receipt)?;
        ensure!(receipt.outcome == Outcome::Completed);
        ensure!(harness.count("tenancy_execution_receipts").await? == 1);
        ensure!(harness.provider.calls.load(Ordering::SeqCst) == 1);
        tokio::time::timeout(WAIT, harness.fixture.service.shutdown()).await??;
        let immutable = serde_json::to_vec(&receipt)?;
        ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == immutable);
        drop((response, http));
        harness.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn incremental_disconnect_during_receipt_commit_keeps_same_shutdown_h1_h2() -> Result<()> {
    for &h2 in modes() {
        let mut harness = Harness::new(Protocol::Chat, None).await?;
        let http = client(h2)?;
        let request = harness.request(&http, true);
        let sending = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
        harness.provider.accepted().await?;
        harness.provider.open.cancel();
        let mut response = tokio::time::timeout(WAIT, sending).await???;
        ensure!(response.status() == StatusCode::OK);
        let id = identity(&response, h2)?;
        let mut seen = prefix(&mut response).await?;
        let gate = harness.fixture.service.cleanup.hold(Phase::Committing);
        let release = Release(harness.fixture.service.clone());
        harness.provider.tail.cancel();
        tokio::time::timeout(WAIT, gate.entered.notified()).await?;
        provisional(&mut response, &mut seen).await?;
        ensure!(evidence(&seen).final_frames == 0);
        // Drop the actual downstream body/client, not a Router::oneshot waiter.
        drop((response, http));
        let mut shutdown = Box::pin(harness.fixture.service.shutdown());
        let held: Result<()> = async {
            ensure!(
                tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
                    .await
                    .is_err()
            );
            ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(
                harness
                    .fixture
                    .service
                    .admission
                    .wait_drained()
                    .now_or_never()
                    .is_none()
            );
            ensure!(!harness.fixture.service.commands.is_closed());
            Ok(())
        }
        .await;
        drop(release);
        held?;
        tokio::time::timeout(WAIT, &mut shutdown).await??;
        drop(shutdown);
        tokio::time::timeout(WAIT, harness.fixture.service.admission.wait_drained()).await?;
        let receipt = harness.receipt(&id).await?;
        completed(&receipt)?;
        ensure!(harness.count("tenancy_execution_receipts").await? == 1);
        ensure!(harness.provider.calls.load(Ordering::SeqCst) == 1);
        let immutable = serde_json::to_vec(&receipt)?;
        harness.fixture.service.shutdown().await?;
        ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == immutable);
        harness.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn receipt_refusal_after_real_prefix_compensates034_json_sse_h1_h2() -> Result<()> {
    for &h2 in modes() {
        for streaming in [false, true] {
            let mut harness = Harness::new(Protocol::Responses, None).await?;
            install_publication_audit(&harness).await?;
            harness.fixture.db.execute_unprepared("CREATE TRIGGER receipt035_wire_refusal BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ABORT, 'receipt wire fixture'); END").await?;
            let http = client(h2)?;
            let request = harness.request(&http, streaming);
            let sending = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
            harness.provider.accepted().await?;
            let ticket = publication_ticket(&harness).await?;
            harness.provider.open.cancel();
            let mut response = tokio::time::timeout(WAIT, sending).await???;
            let id = identity(&response, h2)?;
            let attempted: Result<()> = async {
                ensure!(
                    id == ticket.execution,
                    "HTTP response changed the observed execution"
                );
                let mut seen = if streaming {
                    ensure!(response.status() == StatusCode::OK);
                    let seen = prefix(&mut response).await?;
                    harness.provider.tail.cancel();
                    seen
                } else {
                    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
                    Vec::new()
                };
                finish(&mut response, &mut seen).await?;
                ensure!(!has_success(&seen), "terminal escaped refused receipt");
                ensure!(
                    !streaming || has_error(&seen),
                    "receipt refusal silently truncated the SSE response"
                );
                ensure!(streaming || !String::from_utf8_lossy(&seen).contains("owned reply"));
                ensure!(harness.provider.calls.load(Ordering::SeqCst) == 1);
                ensure!(
                    store::lookup(
                        &harness.fixture.db,
                        &harness.fixture.installation,
                        &harness.fixture.tenant,
                        &id
                    )
                    .await?
                    .is_none()
                );
                // Existing034 activation must compensate when035 refuses its
                // terminal barrier. This first drain remains visibly failed
                // until the same immutable receipt can be reconciled.
                ensure!(
                    tokio::time::timeout(WAIT, harness.fixture.service.shutdown())
                        .await?
                        .is_err()
                );
                publication_compensated(&harness, &ticket).await?;
                ensure!(harness.count("provider_continuations").await? == 0);
                ensure!(
                    harness
                        .fixture
                        .service
                        .http_continuation
                        .state
                        .pending_tickets()
                        .map_err(|_| anyhow::anyhow!("HTTP publication state"))?
                        .is_empty()
                );
                ensure!(
                    harness
                        .fixture
                        .service
                        .admission
                        .wait_drained()
                        .now_or_never()
                        .is_none()
                );
                harness.fixture.service.cleanup.ensure_clean()?;
                Ok(())
            }
            .await;
            // Remove only this isolated fixture's constraint on both outcomes;
            // retrying cleanup reconciles evidence, never the provider request.
            let removed = harness
                .fixture
                .db
                .execute_unprepared("DROP TRIGGER receipt035_wire_refusal")
                .await;
            let drained = tokio::time::timeout(WAIT, harness.fixture.service.shutdown()).await;
            removed?;
            drained??;
            attempted?;
            let receipt = harness.receipt(&id).await?;
            completed(&receipt)?;
            ensure!(harness.count("tenancy_execution_receipts").await? == 1);
            ensure!(harness.count("provider_continuations").await? == 0);
            ensure!(harness.fixture.service.http_continuation.state.barrier());
            let immutable = serde_json::to_vec(&receipt)?;
            harness.fixture.service.shutdown().await?;
            ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == immutable);
            publication_compensated(&harness, &ticket).await?;
            ensure!(harness.provider.calls.load(Ordering::SeqCst) == 1);
            drop((response, http));
            harness.close().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn receipt_refusal_after_provisional_chat_frames_is_failure_h1_h2() -> Result<()> {
    for &h2 in modes() {
        let mut harness = Harness::new(Protocol::Chat, None).await?;
        harness.fixture.db.execute_unprepared("CREATE TRIGGER receipt035_chat_refusal BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ABORT, 'receipt chat fixture'); END").await?;
        let http = client(h2)?;
        let request = harness.request(&http, true);
        let sending = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
        harness.provider.accepted().await?;
        harness.provider.open.cancel();
        let mut response = tokio::time::timeout(WAIT, sending).await???;
        ensure!(response.status() == StatusCode::OK);
        let id = identity(&response, h2)?;
        let mut seen = prefix(&mut response).await?;
        let gate = harness.fixture.service.cleanup.hold(Phase::RollingBack);
        let release = Release(harness.fixture.service.clone());
        harness.provider.tail.cancel();
        let held: Result<()> = async {
            tokio::time::timeout(WAIT, gate.entered.notified()).await?;
            ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(
                harness
                    .fixture
                    .service
                    .admission
                    .wait_drained()
                    .now_or_never()
                    .is_none()
            );
            // The provider finish and usage are valid provisional facts even
            // while the receipt INSERT has failed and rollback is pending.
            held_terminal(&mut response, &mut seen).await
        }
        .await;
        drop(release);
        let attempted: Result<()> = async {
            held?;
            finish(&mut response, &mut seen).await?;
            let observed = evidence(&seen);
            ensure!(observed.provisional_finish && observed.provisional_usage);
            ensure!(
                observed.failed && !observed.successful_final,
                "receipt refusal became a successful final acknowledgement"
            );
            // The existing Chat error encoder closes error + [DONE]. A DONE
            // following an error is failure framing, never a success ACK. An
            // earlier successful DONE stays latched and cannot be erased.
            ensure!(observed.final_frames == 0 || observed.error_closed);
            ensure!(harness.provider.calls.load(Ordering::SeqCst) == 1);
            ensure!(
                store::lookup(
                    &harness.fixture.db,
                    &harness.fixture.installation,
                    &harness.fixture.tenant,
                    &id
                )
                .await?
                .is_none()
            );
            ensure!(
                tokio::time::timeout(WAIT, harness.fixture.service.shutdown())
                    .await?
                    .is_err()
            );
            ensure!(
                harness
                    .fixture
                    .service
                    .admission
                    .wait_drained()
                    .now_or_never()
                    .is_none()
            );
            ensure!(harness.count("provider_continuations").await? == 0);
            harness.fixture.service.cleanup.ensure_clean()?;
            Ok(())
        }
        .await;
        // Reconcile only identical evidence after this acknowledged rollback;
        // both outcomes remove this test-owned constraint and attempt cleanup.
        let removed = harness
            .fixture
            .db
            .execute_unprepared("DROP TRIGGER receipt035_chat_refusal")
            .await;
        let drained = tokio::time::timeout(WAIT, harness.fixture.service.shutdown()).await;
        removed?;
        drained??;
        attempted?;
        let receipt = harness.receipt(&id).await?;
        completed(&receipt)?;
        ensure!(harness.count("tenancy_execution_receipts").await? == 1);
        ensure!(harness.provider.calls.load(Ordering::SeqCst) == 1);
        let immutable = serde_json::to_vec(&receipt)?;
        harness.fixture.service.shutdown().await?;
        ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == immutable);
        drop((response, http));
        harness.close().await?;
    }
    Ok(())
}
