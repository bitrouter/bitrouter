//! Bounded P1-A05 candidates: public authority changes across stock replicas.
//!
//! The only direct SQL mutation is an explicitly labelled, reversible storage
//! fault in the UUID-owned database. No backend, role or shared service is stopped.

use bitrouter_ai::gateway::operator as admin;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use super::*;

const REPLACEMENT: &str = "fixture-authority-account-replacement-v2";

#[derive(Clone, Copy)]
enum Case {
    Revisions,
    InvalidAuthority,
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_authorization_publication_and_credentials_converge_across_replicas() -> Result<()>
{
    run(Case::Revisions).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_loaded_replicas_fail_closed_during_owned_authority_invalidation() -> Result<()> {
    run(Case::InvalidAuthority).await
}

async fn run(case: Case) -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let mut expected = ExitExpectations::new();
    let result = campaign(&mut fixture, &mut expected, case).await;
    finish_empty_fixture_with_database(database, fixture, result, &expected).await
}

async fn campaign(
    fixture: &mut Fixture,
    expected: &mut ExitExpectations,
    case: Case,
) -> Result<()> {
    let mut mock = Mock::start().await?;
    let calls = mock.state.calls.clone();
    let expected_calls = match case {
        Case::Revisions => 13,
        Case::InvalidAuthority => 4,
    };
    let mut jobs = Requests::new();
    // Intent is retained before the fault write, including an uncertain result
    // or timeout. Cleanup never depends on the timed future returning this value.
    let mut fault: Option<AuthorityFault> = None;
    let exercise = async {
        let world = authority_world(fixture, &mock.base, expected).await?;
        match case {
            Case::Revisions => revisions(&world, &mut mock, &mut jobs).await?,
            Case::InvalidAuthority => {
                invalid_authority(fixture, &world, &mut mock, &mut jobs, &mut fault).await?;
            }
        }
        ensure!(jobs.is_empty(), "authority campaign left a client unjoined");
        for child in world.children {
            ensure!(
                fixture.child(child)?.child.try_wait()?.is_none(),
                "serving child exited"
            );
        }
        fixture.no_local_files()?;
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(60), exercise)
        .await
        .context("authority process campaign exceeded its deadline")
        .and_then(|result| result);

    // All owners outlive the exercise. Restore authority before stock children
    // drain, even when the campaign or the first restoration failed.
    let completed_campaign = result.is_ok();
    jobs.abort_all();
    let mut cleanup = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        match joined {
            Ok(Ok((label, _))) => cleanup.push(Err(anyhow::anyhow!("unconsumed client {label}"))),
            Ok(Err(error)) => cleanup.push(Err(error).context("joining authority client")),
            Err(error) if error.is_cancelled() => {}
            Err(error) => cleanup.push(Err(error).context("joining authority client task")),
        }
    }
    if let Some(fault) = &fault {
        cleanup.push(
            fault
                .restore(fixture)
                .await
                .context("restoring owned authority"),
        );
    }
    cleanup.push(mock.finish().await.context("joining counted upstream"));
    if completed_campaign && calls.load(Ordering::SeqCst) != expected_calls {
        cleanup.push(Err(anyhow::anyhow!(
            "upstream dispatch count changed before owned mock join: expected {expected_calls}, got {}",
            calls.load(Ordering::SeqCst),
        )));
    }
    combine_results(result, cleanup)
}

// World::start intentionally grants only read/policy_write. Keep its callers
// unchanged: this child-local launcher adds precisely the grants exercised here,
// retaining the same stock bootstrap, World, child indices and exit accounting.
async fn authority_world(
    fixture: &mut Fixture,
    base: &str,
    expected: &mut ExitExpectations,
) -> Result<World> {
    let keys = std::array::from_fn(|_| crate::auth::keys::generate());
    seed(fixture, &seed_plan(base, &keys)).await?;
    let mut replicas = Vec::new();
    let mut children = Vec::new();
    let mut operators = Vec::new();
    for _ in 0..2 {
        let inference = available_address()?;
        let operator = loop {
            let address = available_address()?;
            if address != inference {
                break address;
            }
        };
        let child = spawn_authority(fixture, inference, operator).await?;
        expected.insert(child, ExpectedExit::CleanServe);
        children.push(child);
        fixture.child(child)?.ready().await?;
        replicas.push(inference);
        operators.push(OperatorClient::new(
            &format!("http://{operator}"),
            OPERATOR_TOKEN,
        )?);
    }
    Ok(World {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .build()?,
        replicas: replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing replicas"))?,
        children: children
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing children"))?,
        operators: operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing operators"))?,
        keys,
    })
}

async fn spawn_authority(
    fixture: &mut Fixture,
    inference: SocketAddr,
    operator: SocketAddr,
) -> Result<usize> {
    let index = fixture.children.len();
    let config = fixture.root.path().join(format!("gateway-{index}.json"));
    tokio::fs::write(&config, serde_json::to_vec(&json!({
        "server":{"listen":inference.to_string(),"log_level":"info"},
        "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
        "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
            "deployment_key_env":"FIXTURE_SHARED_KEY","pool_connections":3,
            "transaction_timeout_ms":1000,"owner_lease_ms":60000,
            "heartbeat_ms":250,"worker_interval_ms":100,"drain_timeout_ms":2000},
        "operator":{"listen":operator.to_string(),"credentials":[{
            "id":"fixture-operator","token_env":"FIXTURE_OPERATOR_TOKEN",
            "grants":["read","policy_write","global_principal_write","credential_write","publish"]}]}
    }))?).await?;
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let child = Command::new(&fixture.binary)
        .args(["serve", "--config"])
        .arg(config)
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env("FIXTURE_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address: inference,
    });
    Ok(index)
}

async fn apply(
    w: &World,
    replica: usize,
    id: &str,
    command: admin::Command,
) -> Result<admin::Receipt> {
    let mutation = Mutation {
        operation_id: id.into(),
        command,
    };
    let receipt = w.operators[replica].apply(&mutation).await?;
    ensure!(
        matches!(w.operators[1-replica].operation(id).await?,
        OperationLookup::Committed { receipt: other } if *other == receipt),
        "operator acknowledgement is not visible on the other replica"
    );
    Ok(receipt)
}

async fn set_active(w: &World, active: bool, operation: &str) -> Result<()> {
    let before = w.operators[0].principal("fixture-principal-0").await?;
    let receipt = apply(
        w,
        1,
        operation,
        admin::Command::PutPrincipal(admin::PrincipalPut {
            principal_id: before.principal_id.clone(),
            tenant_id: before.tenant_id.clone(),
            expected_authorization_revision: Some(before.authorization_revision.clone()),
            key_hash: None,
            active,
            expires_at_ms: before.expires_at_ms,
            requests_per_minute: before.requests_per_minute,
            policy_id: before.policy_id.clone(),
        }),
    )
    .await?;
    let admin::Outcome::Principal(changed) = receipt.outcome else {
        anyhow::bail!("wrong principal receipt");
    };
    ensure!(
        changed.active == active && changed.authorization_revision != before.authorization_revision
    );
    for operator in &w.operators {
        ensure!(operator.principal(&before.principal_id).await? == changed);
    }
    Ok(())
}

async fn deny(
    w: &World,
    mock: &Mock,
    replica: usize,
    principal: usize,
    label: &str,
    expected: StatusCode,
) -> Result<()> {
    let calls = mock.calls();
    let response = Wire::read(
        w.post(replica, principal, "tenant:mock", label, label, label)
            .send()
            .await?,
    )
    .await?;
    response.expect_status(expected)?;
    ensure!(
        mock.calls() == calls,
        "rejected authority request dispatched upstream"
    );
    if expected != StatusCode::SERVICE_UNAVAILABLE {
        ensure!(
            !response.headers.contains_key("x-bitrouter-execution-id"),
            "definite admission rejection created work"
        );
    }
    Ok(())
}

async fn success(
    w: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    replica: usize,
    principal: usize,
    route: (&str, &str, &str),
    label: &str,
) -> Result<GatewayExecution> {
    let (model, account, secret) = route;
    let calls = mock.calls();
    start(
        jobs,
        label,
        w.post(replica, principal, model, label, label, label),
    );
    let arrival = mock.arrival(label, account, secret).await?;
    let id = arrival.execution.clone();
    arrival.release(Reply::Success)?;
    let wire = completed(jobs, label).await?;
    wire.expect_status(StatusCode::OK)?;
    wire.identity(&id, label)?;
    let view = w.wait_state(1 - replica, principal, &id, succeeded).await?;
    ensure!(
        view.attempts.len() == 1 && mock.calls() == calls + 1,
        "success replayed or omitted provider dispatch"
    );
    Ok(view)
}

async fn tenant_status(w: &World, id: &str) -> Result<()> {
    for replica in 0..2 {
        for principal in [0, 1] {
            w.status(replica, principal, id)
                .await?
                .expect_status(StatusCode::OK)?;
        }
        let foreign = w.status(replica, 3, id).await?;
        foreign.expect_status(StatusCode::NOT_FOUND)?;
        ensure!(!foreign.headers.contains_key("x-bitrouter-execution-id"));
        ensure!(
            !String::from_utf8_lossy(&foreign.body).contains(id),
            "foreign status leaked execution identity"
        );
    }
    Ok(())
}

async fn revisions(w: &World, mock: &mut Mock, jobs: &mut Requests) -> Result<()> {
    let initial = w.operators[0].revision().await?;
    ensure!(initial.config_generation == 1);
    let original = success(
        w,
        mock,
        jobs,
        0,
        0,
        ("tenant:mock", "tenant", SECRETS[0]),
        "authority-original",
    )
    .await?;
    success(
        w,
        mock,
        jobs,
        1,
        0,
        ("tenant:mock", "tenant", SECRETS[0]),
        "authority-warm-peer",
    )
    .await?;
    tenant_status(w, &original.execution_id).await?;

    set_active(w, false, "revoke-inference-principal").await?;
    for replica in 0..2 {
        deny(
            w,
            mock,
            replica,
            0,
            &format!("revoked-{replica}"),
            StatusCode::UNAUTHORIZED,
        )
        .await?;
        w.status(replica, 0, &original.execution_id)
            .await?
            .expect_status(StatusCode::UNAUTHORIZED)?;
        // Status is deliberately tenant-scoped, not restricted to its creator.
        w.status(replica, 1, &original.execution_id)
            .await?
            .expect_status(StatusCode::OK)?;
    }
    success(
        w,
        mock,
        jobs,
        0,
        1,
        ("tenant:mock", "tenant", SECRETS[0]),
        "other-principal-still-active",
    )
    .await?;
    set_active(w, true, "restore-inference-principal").await?;
    success(
        w,
        mock,
        jobs,
        1,
        0,
        ("tenant:mock", "tenant", SECRETS[0]),
        "restored-principal",
    )
    .await?;

    let tenant = w.operators[0].tenant("limited").await?;
    let mut disabled = tenant.limits.clone();
    disabled.concurrent_executions = 0;
    let receipt = apply(
        w,
        0,
        "disable-tenant-admission",
        admin::Command::PutTenant(TenantPut {
            tenant_id: tenant.tenant_id.clone(),
            expected_revision: Some(tenant.revision),
            limits: disabled,
        }),
    )
    .await?;
    let admin::Outcome::Tenant(disabled) = receipt.outcome else {
        anyhow::bail!("wrong tenant receipt");
    };
    ensure!(disabled.revision == tenant.revision + 1);
    for replica in 0..2 {
        ensure!(w.operators[replica].tenant("limited").await? == disabled);
        for principal in [0, 1] {
            deny(
                w,
                mock,
                replica,
                principal,
                &format!("disabled-{replica}-{principal}"),
                StatusCode::TOO_MANY_REQUESTS,
            )
            .await?;
        }
    }
    // Zero capacity disables new admission; it does not revoke status authority.
    tenant_status(w, &original.execution_id).await?;
    success(
        w,
        mock,
        jobs,
        1,
        2,
        ("tenant:mock", "tenant", SECRETS[0]),
        "other-tenant-progress",
    )
    .await?;
    apply(
        w,
        1,
        "restore-tenant-admission",
        admin::Command::PutTenant(TenantPut {
            tenant_id: tenant.tenant_id,
            expected_revision: Some(disabled.revision),
            limits: tenant.limits,
        }),
    )
    .await?;
    for replica in 0..2 {
        success(
            w,
            mock,
            jobs,
            replica,
            0,
            ("tenant:mock", "tenant", SECRETS[0]),
            &format!("tenant-restored-{replica}"),
        )
        .await?;
    }

    // Warm both aliases before replacing their one shared credential slot.
    let before = w.operators[0].credential("fixture", "account").await?;
    for (replica, model) in ["account-a:mock", "account-b:mock"].into_iter().enumerate() {
        let view = success(
            w,
            mock,
            jobs,
            replica,
            2,
            (model, "account", SECRETS[1]),
            &format!("credential-before-{replica}"),
        )
        .await?;
        ensure!(view.attempts[0].credential_revision == before.version);
        ensure!(view.attempts[0].resource_pool_id == ACCOUNT_POOL);
    }
    let receipt = apply(
        w,
        1,
        "replace-shared-account",
        admin::Command::ImportCredential(admin::CredentialImport {
            provider: "fixture".into(),
            account: "account".into(),
            resource_pool_id: ACCOUNT_POOL.into(),
            expected_version: Some(before.version),
            credential: admin::CredentialInput::ApiKey {
                value: REPLACEMENT.into(),
            },
        }),
    )
    .await?;
    let admin::Outcome::Credential(changed) = receipt.outcome else {
        anyhow::bail!("wrong credential receipt");
    };
    ensure!(
        changed.version == before.version + 1
            && changed.resource_pool_id == before.resource_pool_id
    );
    for (replica, model) in ["account-a:mock", "account-b:mock"].into_iter().enumerate() {
        ensure!(
            w.operators[replica]
                .credential("fixture", "account")
                .await?
                == changed
        );
        ensure!(
            w.operators[replica].revision().await? == initial,
            "credential import unexpectedly republished configuration"
        );
        let view = success(
            w,
            mock,
            jobs,
            replica,
            2,
            (model, "account", REPLACEMENT),
            &format!("credential-after-{replica}"),
        )
        .await?;
        ensure!(view.attempts[0].credential_revision == changed.version);
        ensure!(view.attempts[0].resource_pool_id == ACCOUNT_POOL);
    }

    let mut publication: admin::Publication =
        serde_json::from_value(seed_plan(&mock.base, &w.keys)["configuration"].clone())?;
    publication.routing["providers"]["tenant"]["api_base"] = json!(format!("{}/other", mock.base));
    let binding = publication
        .credentials
        .iter_mut()
        .find(|binding| binding.provider == "tenant")
        .context("tenant publication binding missing")?;
    binding.credential_account = "other".into();
    let receipt = apply(
        w,
        0,
        "publish-new-tenant-route",
        admin::Command::Publish(admin::PublicationPut {
            expected_generation: initial.config_generation,
            configuration: publication,
        }),
    )
    .await?;
    let admin::Outcome::Published(published) = receipt.outcome else {
        anyhow::bail!("wrong publication receipt");
    };
    ensure!(
        published.config_generation == initial.config_generation + 1
            && published.key_generation == initial.key_generation
    );
    for replica in 0..2 {
        ensure!(w.operators[replica].revision().await? == published);
        new_generation(w, mock, jobs, replica, published.config_generation).await?;
    }
    // Old durable executions retain their original publication and tenant scope.
    for replica in 0..2 {
        let old = w.status(replica, 0, &original.execution_id).await?;
        old.expect_status(StatusCode::OK)?;
        same_facts(&old.execution()?, &original)?;
    }
    tenant_status(w, &original.execution_id).await?;
    Ok(())
}

async fn new_generation(
    w: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    replica: usize,
    generation: u64,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        for probe in 0..64 {
            let label = format!("publication-{replica}-{probe}");
            let calls = mock.calls();
            start(jobs, &label, w.post(replica, 0, "tenant:mock", &label, &label, &label));
            // A stale snapshot may reject with 503 while publication converges.
            // Every dispatch after the committed revision MUST use the new
            // route and credential. A fast swap need not produce a transient 503.
            tokio::select! {
                arrived = mock.arrival(&label, "other", SECRETS[2]) => {
                    let arrival = arrived?;
                    let id = arrival.execution.clone();
                    arrival.release(Reply::Success)?;
                    let response = completed(jobs, &label).await?;
                    response.expect_status(StatusCode::OK)?;
                    response.identity(&id, &label)?;
                    let view = w.wait_state(1-replica, 0, &id, succeeded).await?;
                    ensure!(view.config_generation == generation && view.attempts.len() == 1);
                    ensure!(view.attempts[0].resource_pool_id == OTHER_POOL);
                    ensure!(mock.calls() == calls + 1);
                    return Ok(());
                }
                completed = completed(jobs, &label) => {
                    let response = completed?;
                    response.expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
                    ensure!(!response.headers.contains_key("x-bitrouter-execution-id"), "publication probe failed after admission");
                    ensure!(mock.calls() == calls, "stale publication dispatched upstream");
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("publication never became usable");
    }).await.context("shared publication convergence exceeded its deadline")?
}

async fn health(w: &World, expected: StatusCode) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let mut all = true;
            for replica in w.replicas {
                let status = w
                    .http
                    .get(format!("http://{replica}/health"))
                    .send()
                    .await?
                    .status();
                ensure!(matches!(
                    status,
                    StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
                ));
                all &= status == expected;
            }
            if all {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("replica health did not converge")?
}

async fn invalid_authority(
    fixture: &Fixture,
    w: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    retained: &mut Option<AuthorityFault>,
) -> Result<()> {
    let revision = w.operators[0].revision().await?;
    let original = success(
        w,
        mock,
        jobs,
        0,
        0,
        ("tenant:mock", "tenant", SECRETS[0]),
        "before-authority-fault",
    )
    .await?;
    success(
        w,
        mock,
        jobs,
        1,
        0,
        ("tenant:mock", "tenant", SECRETS[0]),
        "peer-before-authority-fault",
    )
    .await?;
    *retained = Some(AuthorityFault::prepare(fixture).await?);
    let fault = retained
        .as_ref()
        .context("authority fault intent missing")?;
    let result = async {
        fault.install(fixture).await?;
        // These loaded replicas retain their previously valid snapshots. Probe
        // immediately, then require readiness loss; do not assume a scheduled
        // interval in which the old readiness flag is still true.
        for replica in 0..2 {
            deny(
                w,
                mock,
                replica,
                0,
                &format!("invalid-authority-{replica}"),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await?;
            w.status(replica, 0, &original.execution_id)
                .await?
                .expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
        }
        health(w, StatusCode::SERVICE_UNAVAILABLE).await?;
        for replica in 0..2 {
            deny(
                w,
                mock,
                replica,
                0,
                &format!("unready-authority-{replica}"),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    // This is an authority validation fault, not owner expiry or backend recovery.
    // Its budget is shorter than the fixture's 60s owner lease. Restoration also
    // runs outside this timeout and again in campaign cleanup if necessary.
    let result = tokio::time::timeout(Duration::from_secs(8), result)
        .await
        .context("owned authority fault exceeded its bounded window")
        .and_then(|result| result);
    combine_results(result, vec![fault.restore(fixture).await])?;
    *retained = None;
    health(w, StatusCode::OK).await?;
    for replica in 0..2 {
        ensure!(w.operators[replica].revision().await? == revision);
        let restored = w.status(replica, 0, &original.execution_id).await?;
        restored.expect_status(StatusCode::OK)?;
        same_facts(&restored.execution()?, &original)?;
        success(
            w,
            mock,
            jobs,
            replica,
            0,
            ("tenant:mock", "tenant", SECRETS[0]),
            &format!("after-authority-fault-{replica}"),
        )
        .await?;
    }
    tenant_status(w, &original.execution_id).await?;
    Ok(())
}

struct AuthorityFault {
    revision: i64,
    restored_revision: i64,
    original: String,
    invalid: String,
}

impl AuthorityFault {
    async fn prepare(fixture: &Fixture) -> Result<Self> {
        let row = tokio::time::timeout(Duration::from_secs(3), fixture.db.query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT revision, body FROM gateway_shared_records WHERE namespace=$1 AND kind='authority' AND key='head'",
            [fixture.authority.store().namespace().into()],
        ))).await.context("reading owned authority timed out")??.context("owned head missing")?;
        let original: String = row.try_get("", "body")?;
        let revision: i64 = row.try_get("", "revision")?;
        let mut invalid: Value = serde_json::from_str(&original)?;
        ensure!(invalid["format"] == 1);
        invalid["format"] = json!(0);
        Ok(Self {
            revision,
            restored_revision: revision
                .checked_add(1)
                .context("fixture revision exhausted")?,
            original,
            invalid: serde_json::to_string(&invalid)?,
        })
    }

    async fn install(&self, fixture: &Fixture) -> Result<()> {
        // Intentional storage corruption, not an operator-supported mutation.
        // A single bound CAS touches only this fixture's exact authority row.
        // https://www.postgresql.org/docs/current/sql-update.html
        let changed = tokio::time::timeout(Duration::from_secs(3), fixture.db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE gateway_shared_records SET body=$1 WHERE namespace=$2 AND kind='authority' AND key='head' AND revision=$3 AND body=$4",
            [self.invalid.clone().into(), fixture.authority.store().namespace().into(), self.revision.into(), self.original.clone().into()],
        ))).await.context("installing owned authority fault timed out")??;
        ensure!(
            changed.rows_affected() == 1,
            "authority changed before fault installation"
        );
        Ok(())
    }

    async fn restore(&self, fixture: &Fixture) -> Result<()> {
        // Advance only the storage revision, leaving the original configuration
        // and key generations intact. This fences an uncertain install that
        // reaches PostgreSQL AFTER restoration: its original-revision CAS can
        // no longer write. A repeated restore must match our exact restored row.
        // https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED
        let restored = tokio::time::timeout(Duration::from_secs(3), fixture.db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE gateway_shared_records SET body=$1, revision=$5 WHERE namespace=$2 AND kind='authority' AND key='head' AND ((revision=$3 AND (body=$4 OR body=$1)) OR (revision=$5 AND body=$1))",
            [self.original.clone().into(), fixture.authority.store().namespace().into(), self.revision.into(), self.invalid.clone().into(), self.restored_revision.into()],
        ))).await.context("restoring owned authority timed out")??;
        ensure!(
            restored.rows_affected() == 1,
            "owned authority restore CAS did not match; no foreign head overwritten"
        );
        Ok(())
    }
}
