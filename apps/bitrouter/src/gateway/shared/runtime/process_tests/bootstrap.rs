//! Stock bootstrap on a newly created, empty PostgreSQL database.

#[path = "admission.rs"]
mod admission;

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use super::cleanup::{ExitExpectations, ExpectedExit, finish_children, retain_failed_logs};
use super::*;
use crate::gateway::shared::test_database::{EmptyDatabase, combine_results};
use bitrouter_ai::gateway::operator::BootstrapReceipt;

async fn empty_fixture(database: &mut EmptyDatabase) -> Result<Fixture> {
    let binary = PathBuf::from(std::env::var("BITROUTER_TEST_GATEWAY_BINARY")?);
    ensure!(
        binary.is_absolute() && binary.is_file(),
        "an absolute official gateway binary is required"
    );
    let db: DatabaseConnection = database.connect(2, Duration::from_secs(3)).await?;
    let prepared = async {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS count FROM pg_tables WHERE schemaname = 'public'"
                    .to_owned(),
            ))
            .await?
            .context("missing empty database check")?;
        ensure!(
            row.try_get::<i64>("", "count")? == 0,
            "bootstrap database {} is not empty",
            database.name
        );
        let namespace = format!("bootstrap-process-{}", uuid::Uuid::new_v4());
        let store = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
        let key = rand::random();
        let authority = Authority::new(store, DeploymentKey::from_bytes(key)?);
        let root = tempfile::tempdir()?;
        tokio::fs::create_dir(root.path().join("launch")).await?;
        Ok::<_, anyhow::Error>((authority, key, root))
    }
    .await;
    let (authority, key, root) = prepared?;
    Ok(Fixture {
        root,
        binary,
        db,
        authority,
        key,
        url: database.url.clone(),
        children: Vec::new(),
    })
}

// The shared database owner closes the registered pool after these children
// are joined. Aggregate every child error with the campaign error before then.
async fn finish_empty_fixture(mut fixture: Fixture, result: Result<()>) -> Result<()> {
    let prefix = failure_log_prefix();
    finish_children(
        &mut fixture.children,
        result,
        &ExitExpectations::new(),
        &prefix,
    )
    .await
}

fn failure_log_prefix() -> PathBuf {
    std::env::temp_dir().join(format!(
        "bitrouter-process-cleanup-failure-{}",
        uuid::Uuid::new_v4(),
    ))
}

async fn finish_empty_fixture_with_database(
    database: EmptyDatabase,
    mut fixture: Fixture,
    result: Result<()>,
    expected: &ExitExpectations,
) -> Result<()> {
    // Capture final logs AFTER every child wait, including failures discovered
    // only during cleanup. Admission also retains its earlier diagnostic copy.
    let prefix = failure_log_prefix();
    let result = finish_children(&mut fixture.children, result, expected, &prefix).await;
    let already_retained = result.is_err();
    // Keep the fixture's TempDir through DB teardown. EmptyDatabase closes its
    // registered shared pool by reference; this Fixture clone is not a checkout.
    let result = database.finish(result).await;
    if already_retained {
        result
    } else {
        retain_failed_logs(
            fixture.children.iter().map(|child| child.log.as_path()),
            result,
            &prefix,
        )
    }
}

async fn spawn_init(
    fixture: &mut Fixture,
    config: &PathBuf,
    plan: &PathBuf,
) -> Result<(usize, PathBuf)> {
    let index = fixture.children.len();
    let log = fixture
        .root
        .path()
        .join(format!("gateway-init-{index}.log"));
    let report = fixture
        .root
        .path()
        .join(format!("gateway-init-{index}.json"));
    let child = Command::new(&fixture.binary)
        .args(["shared", "init", "--config"])
        .arg(config)
        .arg("--plan")
        .arg(plan)
        .args([
            "--operator-id",
            "bootstrap-fixture",
            "--operation-id",
            "first-seed",
        ])
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(&report)?))
        .stderr(Stdio::from(std::fs::File::create(&log)?))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address: "127.0.0.1:0".parse()?,
    });
    Ok((index, report))
}

async fn completed_init(
    fixture: &mut Fixture,
    child: (usize, PathBuf),
) -> Result<BootstrapReceipt> {
    let running = fixture.child(child.0)?;
    ensure!(
        running.exited().await?.success(),
        "stock bootstrap failed: {}",
        running.logs()
    );
    Ok(serde_json::from_slice(&tokio::fs::read(child.1).await?)?)
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_bootstrap_empty_database_and_join_two_replicas() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = async {
        let mut fixture = empty_fixture(&mut database).await?;
        let result = campaign(&mut fixture).await;
        finish_empty_fixture(fixture, result).await
    }
    .await;
    database.finish(result).await
}

async fn campaign(fixture: &mut Fixture) -> Result<()> {
    let config = fixture.root.path().join("gateway-init-config.json");
    let plan = fixture.root.path().join("gateway-seed.json");
    tokio::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "database": {"url": "${FIXTURE_SHARED_DATABASE}"},
            "gateway_state": {"mode":"shared", "namespace":fixture.authority.store().namespace(),
                "deployment_key_env":"FIXTURE_SHARED_KEY", "transaction_timeout_ms":5000}
        }))?,
    )
    .await?;
    tokio::fs::write(
        &plan,
        serde_json::to_vec(&json!({"configuration": {
            "format":1, "routing":{}, "policies":[], "policy_lock":null,
            "credentials":[], "require_known_pricing":false
        }}))?,
    )
    .await?;

    // Both official processes race first-time schema installation and the same
    // root operation. Neither this fixture nor a serving replica runs migrations.
    let first = spawn_init(fixture, &config, &plan).await?;
    let second = spawn_init(fixture, &config, &plan).await?;
    let receipt = completed_init(fixture, first).await?;
    ensure!(receipt == completed_init(fixture, second).await?);
    ensure!(receipt.revision.config_generation == 1 && receipt.revision.key_generation == 1);
    ensure!(receipt.operations.len() == 1 && receipt.operation_id == "first-seed");
    ensure!(
        fixture.owners().await?.is_empty(),
        "initializer created a serving incarnation"
    );
    ensure!(fixture.authority.head().await?.config_generation == 1);
    let repeated = spawn_init(fixture, &config, &plan).await?;
    ensure!(receipt == completed_init(fixture, repeated).await?);

    let first = fixture.spawn("serve", available_address()?, &[]).await?;
    fixture.child(first)?.ready().await?;
    let second = fixture.spawn("serve", available_address()?, &[]).await?;
    fixture.child(second)?.ready().await?;
    let owners = fixture.owners().await?;
    ensure!(owners.len() == 2 && owners.iter().all(|owner| owner["accepting"] == true));
    ensure!(fixture.child(first)?.stop().await?.success());
    fixture.child(second)?.ready().await?;
    ensure!(fixture.child(second)?.stop().await?.success());
    ensure!(
        fixture
            .owners()
            .await?
            .iter()
            .all(|owner| owner["accepting"] == false)
    );
    fixture.no_local_files()
}
