//! Stock bootstrap on a newly created, empty PostgreSQL database.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use super::*;
use bitrouter_ai::gateway::operator::BootstrapReceipt;

struct EmptyDatabase {
    admin: DatabaseConnection,
    name: String,
    url: String,
}

impl EmptyDatabase {
    async fn create() -> Result<Self> {
        let source = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
        let admin = store::connect(&source, 1, Duration::from_secs(3)).await?;
        let name = format!("bitrouter_bootstrap_{}", uuid::Uuid::new_v4().simple());
        let prepared = async {
            let url = owned_database_url(&source, &name)?;
            admin
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SET statement_timeout = '10s'".to_owned(),
                ))
                .await?;
            // CREATE DATABASE must run outside a transaction. The identifier is
            // generated entirely here, never taken from user input.
            // https://www.postgresql.org/docs/current/sql-createdatabase.html
            admin
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("CREATE DATABASE \"{name}\" TEMPLATE template0"),
                ))
                .await?;
            Ok::<_, anyhow::Error>(url)
        }
        .await;
        match prepared {
            Ok(url) => Ok(Self { admin, name, url }),
            Err(error) => {
                admin.close().await?;
                Err(error
                    .context("creating dedicated empty bootstrap database (requires CREATEDB)"))
            }
        }
    }

    async fn finish(self, result: Result<()>) -> Result<()> {
        // Only drop the UUID database successfully created by this fixture,
        // after all retained children and its connection pool have been closed.
        // https://www.postgresql.org/docs/current/sql-dropdatabase.html
        let dropped = self
            .admin
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DROP DATABASE \"{}\"", self.name),
            ))
            .await;
        let closed = self.admin.close().await;
        result?;
        dropped?;
        closed?;
        Ok(())
    }
}

fn owned_database_url(source: &str, name: &str) -> Result<String> {
    let mut url = url::Url::parse(source)?;
    let parameters: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "dbname")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !parameters.is_empty() {
        url.query_pairs_mut().extend_pairs(parameters);
    }
    url.set_path(name);
    Ok(url.into())
}

#[test]
fn fixture_database_cannot_be_overridden_by_source_query() -> Result<()> {
    let url = url::Url::parse(&owned_database_url(
        "postgres://fixture:secret@localhost/admin?dbname=original&db%6eame=second&sslmode=disable&host=%2Ftmp",
        "owned_fixture",
    )?)?;
    ensure!(url.path() == "/owned_fixture");
    ensure!(url.query_pairs().all(|(key, _)| key != "dbname"));
    ensure!(
        url.query_pairs()
            .any(|(key, value)| key == "host" && value == "/tmp")
    );
    ensure!(
        url.query_pairs()
            .any(|(key, value)| key == "sslmode" && value == "disable")
    );
    ensure!(url.username() == "fixture" && url.password() == Some("secret"));
    Ok(())
}

async fn empty_fixture(url: &str, expected_database: &str) -> Result<Fixture> {
    let binary = PathBuf::from(std::env::var("BITROUTER_TEST_GATEWAY_BINARY")?);
    ensure!(
        binary.is_absolute() && binary.is_file(),
        "an absolute official gateway binary is required"
    );
    let db = store::connect(url, 2, Duration::from_secs(3)).await?;
    let prepared = async {
        let identity = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_database() AS name".to_owned(),
            ))
            .await?
            .context("missing fixture database identity")?;
        ensure!(
            identity.try_get::<String>("", "name")? == expected_database,
            "bootstrap connection did not reach its fixture-owned database"
        );
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
            "bootstrap database is not empty"
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
    let (authority, key, root) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            db.close().await?;
            return Err(error);
        }
    };
    Ok(Fixture {
        root,
        binary,
        db,
        authority,
        key,
        url: url.into(),
        children: Vec::new(),
    })
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
    let database = EmptyDatabase::create().await?;
    let result = async {
        let mut fixture = empty_fixture(&database.url, &database.name).await?;
        let result = campaign(&mut fixture).await;
        fixture.finish(result).await
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
