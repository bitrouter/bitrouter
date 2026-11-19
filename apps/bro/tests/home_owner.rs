//! Both published halves are produced by the real offline importer.
#[path = "support/binary.rs"]
mod binary;
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command};

fn reject(home: &Path) -> Result<()> {
    let entries = || -> Result<Option<Vec<_>>> {
        let dir = match fs::read_dir(home) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut names = dir
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(Some(names))
    };
    let before = entries()?;
    let id = "2ad653eb-9a51-4999-b59f-3c4b1d65174c";
    let mut commands: Vec<&[&str]> = vec![&["thread", "create"], &["thread", "show", id]];
    if before.is_some() {
        commands.push(&["skills", "explain", "--thread", id, "--turn", id]);
    }
    for args in commands {
        let output = Command::new(binary::bro())
            .env("BRO_HOME", home)
            .args(args)
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        ensure!(
            !output.status.success() && stderr.contains("gateway home"),
            "BRO {args:?} should reject {} as a gateway home: {stderr}",
            home.display()
        );
        assert_eq!(entries()?, before);
    }
    assert!(!home.join("threads").exists());
    Ok(())
}

#[test]
fn ordinary_gateway_home_is_rejected_before_any_bro_writes() -> Result<()> {
    for (name, bytes) in [
        ("bitrouter.db", b"fixture".as_slice()),
        (".bitrouter-home-role", b"bitrouter\n".as_slice()),
    ] {
        let home = tempfile::tempdir()?;
        let file = home.path().join(name);
        fs::write(&file, bytes)?;
        reject(home.path())?;
        assert_eq!(fs::read(file)?, bytes);
    }
    Ok(())
}

#[test]
fn nested_gateway_home_is_rejected_before_any_bro_writes() -> Result<()> {
    for (name, bytes) in [
        ("bitrouter.db", b"fixture".as_slice()),
        (".bitrouter-home-role", b"bitrouter\n".as_slice()),
    ] {
        let fixture = tempfile::tempdir()?;
        let gateway = fixture.path().join("gateway");
        let existing = gateway.join("existing/nested");
        fs::create_dir_all(&existing)?;
        let file = gateway.join(name);
        fs::write(&file, bytes)?;

        let missing = existing.join("new/bro");
        reject(&missing)?;
        reject(&existing)?;
        #[cfg(unix)]
        {
            let alias = fixture.path().join("alias");
            std::os::unix::fs::symlink(&existing, &alias)?;
            reject(&alias.join("new/bro"))?;
        }
        assert!(fs::read_dir(&existing)?.next().is_none());
        assert_eq!(fs::read(file)?, bytes);
    }
    Ok(())
}

#[cfg(unix)]
#[path = "home_owner/gateway.rs"]
mod gateway;

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires the separately built gateway in BITROUTER_TEST_GATEWAY_BINARY"]
async fn real_staged_and_activated_gateway_halves_reject_bro() -> Result<()> {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::os::unix::fs::PermissionsExt;
    let binary = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY").context("gateway executable")?;
    let fixture = tempfile::tempdir()?;
    gateway::ordinary_and_legacy(&binary, fixture.path()).await?;
    let src = fixture.path().join("source.sqlite");
    fs::File::create_new(&src)?;
    fs::set_permissions(&src, fs::Permissions::from_mode(0o600))?;
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&src)).await?;
    let schema = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bitrouter/src/db/migration/baseline/fixtures/sqlite-current-schema.json");
    let catalog: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(schema)?)?;
    for kind in ["table", "index"] {
        for row in catalog.iter().filter(|row| row["type"] == kind) {
            sqlx::query(row["sql"].as_str().context("schema DDL")?)
                .execute(&mut db)
                .await?;
        }
    }
    sqlx::raw_sql("INSERT INTO seaql_migrations VALUES('m20261118_000001_model_policy_store',1); INSERT INTO bro_runtime_ownership(id,generation) VALUES(1,0)").execute(&mut db).await?;
    db.close().await?;
    for activate in [false, true] {
        let dest = fixture
            .path()
            .join(if activate { "active" } else { "stage" });
        let mut cmd = Command::new(&binary);
        cmd.current_dir(fixture.path())
            .args(["tenancy", "offline", "split-store", "--database"])
            .arg(&src)
            .arg("--destination")
            .arg(&dest);
        if activate {
            cmd.arg("--activate");
        }
        let out = cmd.output()?;
        ensure!(
            out.status.success(),
            "split failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let gateway = dest.join("bitrouter");
        let bytes = fs::read(gateway.join("bitrouter.db"))?;
        reject(&gateway)?;
        assert_eq!(fs::read(gateway.join("bitrouter.db"))?, bytes);
        let bro = dest.join("bro");
        assert_eq!(bro.join("ACTIVATED").is_file(), activate);
        gateway::refuses(&binary, &bro, fixture.path()).await?;
        ensure!(
            Command::new(binary::bro())
                .env("BRO_HOME", bro)
                .args(["thread", "list"])
                .status()?
                .success()
        );
    }
    Ok(())
}
