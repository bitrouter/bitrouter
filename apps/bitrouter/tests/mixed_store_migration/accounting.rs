//! Real sealed read, two authoritative paid bills, and independent historical
//! service reservations. Only a newly admitted turn may call the gateway.
use super::*;
use bitrouter::cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage};
use bitrouter::metering::{entities::requests, store::MeteringStore};
use bitrouter::storage::evolution::{EvolutionStore, digest, records};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde_json::json;

const UNKNOWN: &str = "ordinary-terminal-unknown";
const CONTROLLER: &str = "old-controller";

async fn reservation(db: &DatabaseConnection, id: &str, cost: Option<i64>) -> Result<()> {
    let owner = digest(&("gateway-request-inventory-v1", "local", CONTROLLER))?;
    let store = EvolutionStore::new(db.clone(), &format!("gateway:{owner}"))?;
    let body = json!({"request_id":id,"attempt_id":uuid::Uuid::new_v4(),"route_scope_id":"local","controller_instance_id":CONTROLLER,"runtime_epoch":uuid::Uuid::new_v4(),
        "admitted_at":"2026-10-01T00:00:00Z","anchors":{},"binding":null,"coverage_ready":true,
        "settlement":{"settled_at":"2026-10-01T00:00:01Z","outcome":"completed","hops":[{"provider":"mock","model":"test-model","account":null,"protocol":"chat_completions","status":"completed","error_code":null}],
        "final_model":"test-model","final_provider":"mock","error_code":null,"duration_ms":1000,"final_metered_cost_micro_usd":cost,"total_cost_micro_usd":cost}});
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO checkpoint_evolution(record_id,scope_id,kind,revision,body) VALUES(?,?,'gateway_request',2,?)",[store.id("gateway_request",id)?.into(),store.scope_id.into(),body.to_string().into()])).await?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE requests SET route_scope_id='local', controller_instance_id=? WHERE request_id=?",
        [CONTROLLER.into(), id.into()],
    ))
    .await?;
    Ok(())
}

async fn combined(dir: &Path) -> Result<(PathBuf, DatabaseConnection)> {
    let (path, db) = captured(dir).await?;
    capture::settle(&db, REQUESTS[0], 173).await?;
    capture::settle(&db, REQUESTS[1], 259).await?;
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO requests(request_id,user_id,api_key_id,model_id,provider_id,prompt_tokens,completion_tokens,reasoning_tokens,created_at,charge_status,reconciliation_status) VALUES(?,'local','local','test-model','mock',7,4,0,'2026-10-01T00:00:01Z','unknown','pending')",[UNKNOWN.into()])).await?;
    MeteringStore::new(db.clone())
        .apply_authoritative_receipt_charge(&SettlementReceipt {
            request_id: UNKNOWN.into(),
            state: SettlementState::Unknown,
            model_id: Some("test-model".into()),
            provider_id: Some("mock".into()),
            final_charge_micro_usd: None,
            usage: SettlementUsage {
                uncached_input_tokens: 7,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 4,
                reasoning_tokens: 0,
            },
        })
        .await?;
    // Also keep an ordinary locally metered request for which M0 had no price.
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "ATTACH ? AS past",
        [dir.join("captured.sqlite")
            .to_str()
            .context("captured path")?
            .into()],
    ))
    .await?;
    db.execute_unprepared("INSERT INTO main.requests(request_id,user_id,api_key_id,model_id,provider_id,prompt_tokens,completion_tokens,reasoning_tokens,uncached_input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,usage_origin,raw_usage_json,charge_status,charge_evidence_json,reconciliation_status,estimated_charge_micro_usd,created_at) SELECT 'ordinary-local-unknown',user_id,api_key_id,model_id,provider_id,prompt_tokens,completion_tokens,reasoning_tokens,uncached_input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,usage_origin,raw_usage_json,charge_status,charge_evidence_json,reconciliation_status,estimated_charge_micro_usd,created_at FROM past.requests ORDER BY request_id LIMIT 1").await?;
    db.execute_unprepared("DETACH DATABASE past").await?;
    reservation(&db, REQUESTS[0], Some(173)).await?;
    reservation(&db, UNKNOWN, None).await?;
    Ok((path, db))
}

async fn evidence(db: &DatabaseConnection) -> Result<(Vec<requests::Model>, Vec<records::Model>)> {
    let bills = requests::Entity::find()
        .filter(requests::Column::ProviderId.eq("mock"))
        .order_by_asc(requests::Column::RequestId)
        .all(db)
        .await?;
    let owner = digest(&("gateway-request-inventory-v1", "local", CONTROLLER))?;
    let store = EvolutionStore::new(db.clone(), &format!("gateway:{owner}"))?;
    let reservations = records::Entity::find()
        .filter(records::Column::Kind.eq("gateway_request"))
        .filter(records::Column::ScopeId.eq(store.scope_id))
        .order_by_asc(records::Column::RecordId)
        .all(db)
        .await?;
    ensure!(
        bills.len() == 4 && reservations.len() == 2,
        "incomplete historical ledger"
    );
    Ok((bills, reservations))
}

#[tokio::test]
async fn paid_native_and_terminal_unknown_survive_ordinary_restart() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let (source, db) = combined(fixture.path()).await?;
    let before = evidence(&db).await?;
    db.close().await?;
    let dest = fixture.path().join("activated");
    let result = mixed_store::activate(&source, &dest).await?;
    ensure!(result.dispatch_allowed, "{:?}", result.blockers);
    let projected = result.stores.context("projection")?;
    for _ in 0..2 {
        let service = db::connect(&format!("sqlite://{}", projected.bitrouter.display())).await?;
        ensure!(evidence(&service).await? == before);
        ensure!(scalar(&service,"SELECT sum(estimated_charge_micro_usd) FROM requests WHERE reconciliation_status='computed'").await?==432);
        ensure!(scalar(&service,"SELECT count(*) FROM requests WHERE reconciliation_status='unknown' AND json_extract(authoritative_receipt_json,'$.state')='unknown' AND json_extract(authoritative_receipt_json,'$.final_charge_micro_usd') IS NULL").await?==1);
        service.close().await?;
    }
    let thread: Value = serde_json::from_slice(&fs::read(
        dest.join("bro/threads").join(format!("{THREAD}.json")),
    )?)?;
    let messages = thread["turns"][0]["sealed_messages"]
        .as_array()
        .context("sealed history")?;
    ensure!(
        messages
            .iter()
            .filter(|m| m["role"] == "tool"
                && m["tool_call_id"] == "read-evidence"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("baseline persisted payload")))
            .count()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn local_price_and_exhausted_receipt_retain_their_distinct_bases() -> Result<()> {
    use bitrouter::metering::pricing::{ModelPricing, PricingSource, calculate_charge_evidence};
    use bitrouter_ai::types::{Usage, UsageOrigin};
    for exhausted in [false, true] {
        let dir = tempfile::tempdir()?;
        let (source, db) = combined(dir.path()).await?;
        let request_id = "ordinary-local-unknown";
        if exhausted {
            db.execute_unprepared("UPDATE requests SET reconciliation_status='pending' WHERE request_id='ordinary-local-unknown'").await?;
            let store = MeteringStore::new(db.clone());
            ensure!(store.start_reconciliation_attempt(request_id).await? == 1);
            store
                .exhaust_reconciliation(request_id, "receipt_fetch_attempts_exhausted")
                .await?;
        } else {
            let row = requests::Entity::find_by_id(request_id)
                .one(&db)
                .await?
                .context("local usage")?;
            let usage = Usage {
                prompt_tokens: row.prompt_tokens.try_into()?,
                completion_tokens: row.completion_tokens.try_into()?,
                cache_read_tokens: row.cache_read_tokens.try_into()?,
                cache_write_tokens: row.cache_write_tokens.try_into()?,
                reasoning_tokens: row.reasoning_tokens.try_into()?,
                origin: UsageOrigin::ProviderReported,
                ..Default::default()
            };
            let price = ModelPricing::cache_aware(Some(2.0), Some(1.0), Some(3.0), Some(4.0));
            let computed = calculate_charge_evidence(&usage, &price, PricingSource::Configured);
            let amount = computed.charge_micro_usd.context("computed price")?;
            ensure!(amount > 0);
            db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE requests SET charge_status='computed',charge_evidence_json=?,estimated_charge_micro_usd=? WHERE request_id=?",
                [serde_json::to_string(&computed)?.into(), amount.into(), request_id.into()])).await?;
        }
        let before = evidence(&db).await?;
        db.close().await?;
        let outcome = mixed_store::activate(&source, &dir.path().join("active")).await?;
        let projected = outcome
            .stores
            .with_context(|| format!("{:?}", outcome.blockers))?;
        let service = db::connect(&format!("sqlite://{}", projected.bitrouter.display())).await?;
        ensure!(evidence(&service).await? == before);
        service.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn refuses_in_transit_unbacked_and_conflicting_gateway_facts() -> Result<()> {
    for damage in [
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement',NULL) WHERE kind='gateway_request'",
        "UPDATE checkpoint_evolution SET record_id='wrong' WHERE json_extract(body,'$.request_id')='ordinary-terminal-unknown'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement.total_cost_micro_usd',999) WHERE kind='gateway_request'",
        "DELETE FROM requests WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET reconciliation_status='pending' WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET authoritative_receipt_json=NULL WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET authoritative_receipt_json=NULL,usage_origin='provider_reported',reconciliation_last_error='receipt_fetch_attempts_exhausted' WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET charge_evidence_json=json_set(charge_evidence_json,'$.pricing_source','override') WHERE charge_status='computed'",
        "UPDATE requests SET charge_evidence_json=json_set(charge_evidence_json,'$.normalized_usage.output_tokens',777) WHERE charge_status='computed'",
    ] {
        let dir = tempfile::tempdir()?;
        let (source, db) = combined(dir.path()).await?;
        db.execute_unprepared(damage).await?;
        db.close().await?;
        let dest = dir.path().join("active");
        let result = mixed_store::activate(&source, &dest).await?;
        ensure!(
            !result.dispatch_allowed && !dest.exists(),
            "unexpected activation for {damage}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires the separately built BRO in BITROUTER_TEST_BRO"]
async fn restarted_products_preserve_paid_history_and_refuse_historical_request_replay()
-> Result<()> {
    use std::{net::TcpListener, time::Duration};
    use tokio::process::Command;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let bro = std::env::var_os("BITROUTER_TEST_BRO").context("BRO binary")?;
    let gateway = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_bitrouter").into());
    let dir = tempfile::tempdir()?;
    let (source, db) = combined(dir.path()).await?;
    let before = evidence(&db).await?;
    db.close().await?;
    let outcome = mixed_store::activate(&source, &dir.path().join("active")).await?;
    let pair = outcome
        .stores
        .with_context(|| format!("{:?}", outcome.blockers))?;
    let service_home = pair.bitrouter.parent().context("service home")?;
    let bro_home = pair.bro.parent().context("BRO home")?;
    let work = dir.path().join("workspace");
    fs::create_dir(&work)?;
    let port = TcpListener::bind("127.0.0.1:0")?;
    let address = port.local_addr()?;
    let endpoint = format!("http://{address}");
    let provider = MockServer::start().await;
    let config = service_home.join("bitrouter.yaml");
    fs::write(
        &config,
        format!(
            "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '{address}', skip_auth: true}}\ndatabase: {{url: 'sqlite://bitrouter.db?mode=rwc'}}\nproviders:\n  newly-routed:\n    api_base: '{}'\n    api_key: fixture-only\n    api_protocol:\n      - '*': chat_completions\n    models:\n      - id: test-model\n",
            provider.uri()
        ),
    )?;
    let body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"new-completion","created":1,"model":"test-model","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"continued once"},"finish_reason":"stop"}]})
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&provider)
        .await;
    drop(port);
    let client = reqwest::Client::builder().no_proxy().build()?;
    let run = |args: Vec<String>| {
        let mut cmd = Command::new(&bro);
        cmd.kill_on_drop(true)
            .env_clear()
            .env("HOME", &work)
            .current_dir(&work)
            .arg("--data-dir")
            .arg(bro_home)
            .args(["--endpoint", &endpoint])
            .args(args);
        async move {
            let output = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await??;
            ensure!(
                output.status.success(),
                "BRO: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok::<Value, anyhow::Error>(serde_json::from_slice(
                output
                    .stdout
                    .split(|b| *b == b'\n')
                    .rfind(|l| !l.is_empty())
                    .context("BRO output")?,
            )?)
        }
    };
    let args = |tokens: &[&str]| tokens.iter().map(|s| s.to_string()).collect();
    let mut new_turn = None;
    for restart in 0..2 {
        let mut process = Command::new(&gateway)
            .kill_on_drop(true)
            .env_clear()
            .env("HOME", service_home)
            .env("BITROUTER_HOME", service_home)
            .current_dir(service_home)
            .args(["serve", "--config"])
            .arg(&config)
            .spawn()?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                ensure!(
                    process.try_wait()?.is_none(),
                    "gateway exited before readiness"
                );
                if client
                    .get(format!("{endpoint}/v1/models"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        for old in [REQUESTS[0], UNKNOWN] {
            let response=client.post(format!("{endpoint}/v1/chat/completions")).header("x-bitrouter-request-id",old).header("x-bitrouter-controller-id","different-controller")
                .json(&json!({"model":"test-model","messages":[{"role":"user","content":"must never dispatch"}]})).send().await?;
            ensure!(
                !response.status().is_success(),
                "historical request replay {old} was admitted"
            );
        }
        if restart == 0 {
            run(args(&[
                "managed",
                "restore",
                THREAD,
                "--workspace",
                work.to_str().context("workspace")?,
            ]))
            .await?;
            let accepted = run(args(&[
                "thread",
                "start",
                THREAD,
                "continue after cutover",
                "--request-id",
                "after-cutover",
            ]))
            .await?;
            ensure!(accepted["final_answer"] == "continued once");
            new_turn = Some(accepted["turn_id"].as_str().context("turn")?.to_string());
        } else {
            let id = new_turn.as_deref().context("new turn")?;
            ensure!(
                run(args(&["thread", "resume", THREAD, "--turn", id])).await?["final_answer"]
                    == "continued once"
            );
            ensure!(
                run(args(&[
                    "thread",
                    "start",
                    THREAD,
                    "continue after cutover",
                    "--request-id",
                    "after-cutover"
                ]))
                .await?["turn_id"]
                    == id
            );
        }
        process.kill().await?;
    }
    let seen = provider
        .received_requests()
        .await
        .context("provider history")?;
    ensure!(seen.len() == 1, "{} upstream dispatches", seen.len());
    let sent: Value = seen[0].body_json()?;
    ensure!(
        sent["messages"]
            .as_array()
            .context("messages")?
            .iter()
            .filter(|m| m["tool_call_id"] == "read-evidence"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("baseline persisted payload")))
            .count()
            == 1
    );
    let db = db::connect(&format!("sqlite://{}", pair.bitrouter.display())).await?;
    ensure!(evidence(&db).await? == before);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn retained_native_charge_claims_require_the_exact_backing_evidence() -> Result<()> {
    use bitrouter::gateway::{
        caller::CallerContext, pipeline::native_accounting::NativeCostSource,
    };
    use bitrouter::storage::checkpoint::sha256;
    for corrupt in [false, true] {
        let dir = tempfile::tempdir()?;
        let (path, db) = fresh(dir.path()).await?;
        for (id, amount) in REQUESTS.iter().zip([173, 259]) {
            capture::settle(&db, id, amount).await?;
        }
        let observations = MeteringStore::new(db.clone())
            .read(&CallerContext::local(), &REQUESTS.map(str::to_owned))
            .await?;
        let j = managed_journal_with(|s| {
            let run = s["run"]["run_id"].as_str().context("run ID")?.to_owned();
            for obs in &observations {
                s["cost_work"][&run]["charge_unknown"]
                    .as_object_mut()
                    .context("cost ledger")?
                    .remove(&obs.request_id);
                for claim in &obs.claims {
                    let key = sha256(&serde_json::to_vec(&(
                        &claim.source,
                        &claim.bill_id,
                        claim.basis,
                    ))?);
                    let mut preserved = json!(claim);
                    if corrupt {
                        preserved["micro_usd"] = json!(claim.micro_usd + 1);
                    }
                    s["cost_work"][&run]["charges"][key] = preserved;
                }
            }
            Ok(())
        })?;
        let bytes = serde_json::to_string(&j)?;
        db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO bro_managed_sessions(id,revision,payload_bytes,payload) VALUES('captured-release',95,?,?)",[i64::try_from(bytes.len())?.into(),bytes.into()])).await?;
        db.close().await?;
        let dest = dir.path().join("active");
        let result = mixed_store::activate(&path, &dest).await?;
        ensure!(
            result.dispatch_allowed != corrupt && dest.exists() != corrupt,
            "{:?}",
            result.blockers
        );
    }
    Ok(())
}
