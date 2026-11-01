use super::*;
use crate::offline::{
    format::{Column, EXCLUSIONS, Inventory, Table},
    large::Options,
};
use std::os::unix::fs::PermissionsExt;
use tokio_util::sync::CancellationToken;

fn evidence() -> Evidence {
    Evidence {
        format: FORMAT_V2.into(),
        scope: "sqlite_database_only".into(),
        complete: false,
        cutover_ready: false,
        exclusions: EXCLUSIONS.iter().map(|value| (*value).into()).collect(),
        inspector: crate::artifact::current(),
        source_commit: None,
        source_build: None,
        selected_path_digest: "a".repeat(64),
        key_id: "b".repeat(64),
        snapshot_bytes: 8192,
        snapshot_digest: "c".repeat(64),
        inventory: Inventory {
            encoding: "UTF-8".into(),
            schema_digest: "d".repeat(64),
            tables: (0..80)
                .map(|table| Table {
                    name: format!("table_{table}"),
                    columns: (0..128)
                        .map(|column| Column {
                            name: format!("column_{column}_{}", "x".repeat(196)),
                            primary_key_position: 0,
                        })
                        .collect(),
                    rows: 0,
                    value_bytes: 0,
                    digest: "e".repeat(64),
                })
                .collect(),
            rows: 0,
            value_bytes: 0,
            ledger: None,
            installation_id: None,
        },
    }
}
fn workspace(path: &Path, bytes: usize) -> Result<Workspace> {
    Workspace::create(
        &Options {
            working_bytes: bytes,
            scratch_directory: path.into(),
            min_free_bytes: 1024 * 1024,
        },
        CancellationToken::new(),
    )
}

#[test]
fn large_schema_metadata_uses_working_budget_and_preserves_v1_refusal() -> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let workspace = workspace(home.path(), 128 * 1024 * 1024)?;
    let mut value = evidence();
    let (bytes, charge) = encode(&value, &workspace)?;
    assert!(bytes.len() as u64 > MAX_EVIDENCE);
    let path = home.path().join("evidence.json");
    files::write_new(&path, &bytes)?;
    let (actual, anchor, decoded) = read(&path, &workspace)?;
    assert_eq!(actual.inventory, value.inventory);
    assert_eq!(anchor, files::sha(&bytes));
    assert_eq!(anchor, digest(&path, &workspace)?);
    drop((actual, decoded, charge, bytes));
    value.format = FORMAT.into();
    assert!(encode(&value, &workspace).is_err());
    drop(value);
    workspace.finish()?;
    Ok(())
}

#[test]
fn dense_container_is_refused_by_shape_before_owned_evidence_decode() -> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let workspace = workspace(home.path(), 3 * 1024 * 1024)?;
    let text = format!(
        "{{\"format\":\"{FORMAT_V2}\",\"exclusions\":[{}]}}",
        vec!["\"\""; 60_000].join(",")
    );
    assert!((text.len() * 6 + 8192) < workspace.available());
    let path = home.path().join("dense.json");
    files::write_new(&path, text.as_bytes())?;
    let before = workspace.available();
    let error = match read(&path, &workspace) {
        Ok(_) => anyhow::bail!("dense shape unexpectedly decoded"),
        Err(error) => error,
    };
    assert!(
        error.downcast_ref::<CapacityExceeded>().is_some(),
        "{error}"
    );
    assert_eq!(workspace.available(), before);
    workspace.finish()?;
    Ok(())
}
