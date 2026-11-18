//! Real sealed read, two authoritative paid bills, and independent historical
//! service reservations. Only a newly admitted turn may call the gateway.
use super::*;
use bitrouter::metering::{entities::requests, store::MeteringStore};
use bitrouter::storage::evolution::{EvolutionStore, digest, records};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde_json::json;

const UNKNOWN: &str = "ordinary-terminal-unknown";
const CONTROLLER: &str = "old-controller";

const FROZEN: &[u8] = include_bytes!("../fixtures/m0-gateway-settlement/reconciled.sqlite.zst");
const DISPATCH: &[u8] = include_bytes!("../fixtures/m0-gateway-settlement/dispatch.sqlite.zst");

fn original(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, zstd::stream::decode_all(bytes)?)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

async fn combined(dir: &Path) -> Result<(PathBuf, DatabaseConnection)> {
    let (path, db) = captured(dir).await?;
    capture::settle(&db, REQUESTS[0], 173).await?;
    capture::settle(&db, REQUESTS[1], 259).await?;
    let journal = dir.join("gateway-journal.sqlite");
    original(&journal, FROZEN)?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "ATTACH ? AS original",
        [journal.to_str().context("journal path")?.into()],
    ))
    .await?;
    for table in [
        "requests",
        "checkpoint_evolution",
        "acp_capture_connections",
        "acp_capture_events",
    ] {
        db.execute_unprepared(&format!(
            "INSERT INTO main.{table} SELECT * FROM original.{table}"
        ))
        .await?;
    }
    db.execute_unprepared("DETACH DATABASE original").await?;
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
    Ok((path, db))
}

async fn evidence(db: &DatabaseConnection) -> Result<(Vec<requests::Model>, Vec<records::Model>)> {
    let bills = requests::Entity::find()
        .filter(requests::Column::ProviderId.is_in(["mock", "bitrouter"]))
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
        bills.len() == 6 && reservations.len() == 3,
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
        ensure!(scalar(&service,"SELECT sum(estimated_charge_micro_usd) FROM requests WHERE provider_id='mock' AND reconciliation_status='computed'").await?==432);
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
async fn frozen_gateway_journal_keeps_dispatch_costs_separate_from_later_bills() -> Result<()> {
    use bitrouter::gateway::{
        caller::CallerContext,
        pipeline::native_accounting::{NativeCostBasis, NativeCostSource},
    };
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("reconciled.sqlite");
    let pending = dir.path().join("dispatch.sqlite");
    original(&source, FROZEN)?;
    original(&pending, DISPATCH)?;
    ensure!(
        !mixed_store::activate(&pending, &dir.path().join("too-early"))
            .await?
            .dispatch_allowed
    );
    let before = connection(&pending, true).await?;
    let result = mixed_store::activate(&source, &dir.path().join("ready")).await?;
    let active = result
        .stores
        .with_context(|| format!("{:?}", result.blockers))?;
    let owner = digest(&("gateway-request-inventory-v1", "local", CONTROLLER))?;
    let scope = EvolutionStore::new(before.clone(), &format!("gateway:{owner}"))?.scope_id;
    for _ in 0..2 {
        let service = db::connect(&format!("sqlite://{}", active.bitrouter.display())).await?;
        let historical = |db: DatabaseConnection| {
            let scope = scope.clone();
            async move {
                records::Entity::find()
                    .filter(records::Column::ScopeId.eq(scope))
                    .order_by_asc(records::Column::RecordId)
                    .all(&db)
                    .await
            }
        };
        let journal = historical(service.clone()).await?;
        ensure!(journal.len() == 3 && journal == historical(before.clone()).await?);
        for row in journal {
            let request: Value = serde_json::from_str(&row.body)?;
            let id = request["request_id"].as_str().context("request id")?;
            let settlement = &request["settlement"];
            let (hops, amount) = match id {
                "m0-single-paid" => (1, Some(47)),
                "m0-fallback-paid" => (2, Some(61)),
                UNKNOWN => (1, None),
                _ => anyhow::bail!("unexpected historical request"),
            };
            ensure!(settlement["hops"].as_array().context("hops")?.len() == hops);
            ensure!(
                settlement["final_metered_cost_micro_usd"] == 30
                    && settlement["total_cost_micro_usd"]
                        == if hops == 1 { json!(30) } else { Value::Null }
            );
            let original = requests::Entity::find_by_id(id)
                .one(&before)
                .await?
                .context("original metering")?;
            ensure!(
                original.estimated_charge_micro_usd == 30
                    && original.reconciliation_status == "pending"
            );
            let dispatch = MeteringStore::new(before.clone())
                .read(&CallerContext::local(), &[id.to_owned()])
                .await?;
            let [provisional] = dispatch.as_slice() else {
                anyhow::bail!("missing dispatch evidence");
            };
            ensure!(
                provisional.unknown_reason.is_some()
                    && provisional
                        .claims
                        .iter()
                        .all(|claim| claim.basis == NativeCostBasis::Estimated)
            );
            let observations = MeteringStore::new(service.clone())
                .read(&CallerContext::local(), &[id.to_owned()])
                .await?;
            let [observed] = observations.as_slice() else {
                anyhow::bail!("missing observation")
            };
            let reconciled: Vec<_> = observed
                .claims
                .iter()
                .filter(|c| c.basis == NativeCostBasis::Reconciled)
                .map(|c| c.micro_usd)
                .collect();
            ensure!(
                reconciled == amount.into_iter().collect::<Vec<_>>()
                    && observed.unknown_reason.is_some() == amount.is_none()
            );
        }
        service.close().await?;
    }
    before.close().await?;
    Ok(())
}

#[tokio::test]
async fn refuses_in_transit_unbacked_and_conflicting_gateway_facts() -> Result<()> {
    for damage in [
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement',NULL) WHERE kind='gateway_request'",
        "UPDATE checkpoint_evolution SET record_id='wrong' WHERE json_extract(body,'$.request_id')='ordinary-terminal-unknown'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement.total_cost_micro_usd',999) WHERE kind='gateway_request'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement.hops[0].status','completed') WHERE json_extract(body,'$.request_id')='m0-fallback-paid'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement.total_cost_micro_usd',30) WHERE json_extract(body,'$.request_id')='m0-fallback-paid'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.settlement.hops[1].provider','first') WHERE json_extract(body,'$.request_id')='m0-fallback-paid'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.anchors',json('{}')) WHERE json_extract(body,'$.request_id')='m0-single-paid'",
        "UPDATE checkpoint_evolution SET revision=revision-1 WHERE kind='gateway_coverage'",
        "UPDATE checkpoint_evolution SET body=json_set(body,'$.runtime_epoch','ffffffff-ffff-4fff-8fff-ffffffffffff') WHERE kind='gateway_request'",
        "UPDATE requests SET session_identity_json=json_set(session_identity_json,'$.router_request_id','different') WHERE request_id='m0-fallback-paid'",
        "UPDATE acp_capture_connections SET state='recording'",
        "DELETE FROM requests WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET reconciliation_status='pending' WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET authoritative_receipt_json=NULL WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET authoritative_receipt_json=NULL,usage_origin='provider_reported',reconciliation_last_error='receipt_fetch_attempts_exhausted' WHERE request_id='ordinary-terminal-unknown'",
        "UPDATE requests SET charge_evidence_json=json_set(charge_evidence_json,'$.pricing_source','override') WHERE charge_status='computed'",
        "UPDATE requests SET charge_evidence_json=json_set(charge_evidence_json,'$.normalized_usage.output_tokens',777) WHERE charge_status='computed'",
    ] {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("contradiction.sqlite");
        original(&source, FROZEN)?;
        let db = connection(&source, false).await?;
        ensure!(db.execute_unprepared(damage).await?.rows_affected() > 0);
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
        for old in [REQUESTS[0], "m0-single-paid", "m0-fallback-paid", UNKNOWN] {
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
