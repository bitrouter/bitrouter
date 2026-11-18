//! Both published halves are produced by the real offline importer.
#[path = "support/binary.rs"]
mod binary;
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path, process::Command};

fn reject(home: &Path) -> Result<()> {
    let entries = || -> Result<Vec<_>> {
        let mut names = fs::read_dir(home)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    };
    let before = entries()?;
    let id = "2ad653eb-9a51-4999-b59f-3c4b1d65174c";
    for args in [
        vec!["thread", "create"],
        vec!["thread", "show", id],
        vec!["skills", "explain", "--thread", id, "--turn", id],
    ] {
        let output = Command::new(binary::bro())
            .env("BRO_HOME", home)
            .args(args)
            .output()?;
        ensure!(
            !output.status.success()
                && String::from_utf8_lossy(&output.stderr).contains("gateway home")
        );
        assert_eq!(entries()?, before);
    }
    assert!(!home.join("threads").exists());
    Ok(())
}

#[test]
fn ordinary_gateway_home_is_rejected_before_any_bro_writes() -> Result<()> {
    let home = tempfile::tempdir()?;
    fs::write(home.path().join("bitrouter.db"), b"fixture")?;
    reject(home.path())
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires the separately built gateway in BITROUTER_TEST_GATEWAY_BINARY"]
async fn real_staged_and_activated_gateway_halves_reject_bro() -> Result<()> {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::os::unix::fs::PermissionsExt;
    let binary = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY").context("gateway executable")?;
    let fixture = tempfile::tempdir()?;
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
