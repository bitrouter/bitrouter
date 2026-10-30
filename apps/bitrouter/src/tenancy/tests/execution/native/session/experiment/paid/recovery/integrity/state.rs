//! Bounded raw evidence, following the existing scalar quote() audit convention.
use super::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
};
const MAX: usize = 32 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    schema: Vec<(String, String, String, String)>,
    tables: BTreeMap<String, Table>,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Table {
    definition: String,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Snapshot {
    pub(super) fn check_negative_delta(&self, after: &Self) -> Result<()> {
        ensure!(self != after, "integrity mutation matched no real bytes");
        ensure!(self.schema == after.schema && self.tables.keys().eq(after.tables.keys()));
        for (name, before) in &self.tables {
            let next = after.tables.get(name).context("preserved table")?;
            if name != "checkpoint_evolution" {
                ensure!(before == next, "negative setup changed another table");
                continue;
            }
            ensure!(before.definition == next.definition && before.columns == next.columns);
            ensure!(before.rows.len() == next.rows.len());
            let kind = before
                .columns
                .iter()
                .position(|c| c == "kind")
                .context("kind column")?;
            let retained = |row: &&Vec<String>| {
                !matches!(
                    row[kind].as_str(),
                    "'owned_judge_active_v3'" | "'owned_judge_pending'"
                )
            };
            ensure!(
                before
                    .rows
                    .iter()
                    .filter(retained)
                    .eq(next.rows.iter().filter(retained)),
                "negative setup changed counters, source, history or other records"
            );
        }
        Ok(())
    }
}

fn identifier(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    );
    Ok(format!("\"{name}\""))
}
pub(super) async fn snapshot(db: &DatabaseConnection) -> Result<Snapshot> {
    let bounds = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT count(*) AS n,coalesce(sum(length(CAST(sql AS BLOB))),0) AS bytes FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'".to_owned())).await?.context("schema bounds")?;
    ensure!(
        bounds.try_get::<i64>("", "n")? <= 512 && bounds.try_get::<i64>("", "bytes")? <= MAX as i64
    );
    let definitions = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type COLLATE BINARY,name COLLATE BINARY LIMIT 513".to_owned())).await?;
    ensure!(definitions.len() <= 512);
    let mut schema = Vec::with_capacity(definitions.len());
    for row in definitions {
        schema.push((
            row.try_get("", "type")?,
            row.try_get("", "name")?,
            row.try_get("", "tbl_name")?,
            row.try_get("", "sql")?,
        ));
    }
    let tables = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT name,sql FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name COLLATE BINARY LIMIT 257".to_owned())).await?;
    ensure!(!tables.is_empty() && tables.len() <= 256);
    let mut output = BTreeMap::new();
    let mut count = 0usize;
    let mut bytes = usize::try_from(bounds.try_get::<i64>("", "bytes")?)?;
    for table in tables {
        let name: String = table.try_get("", "name")?;
        let quoted = identifier(&name)?;
        let definition: String = table.try_get("", "sql")?;
        ensure!(definition.len() <= 64 * 1024);
        let fields = db
            .query_all(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT name FROM pragma_table_info(?) ORDER BY cid LIMIT 129",
                [name.clone().into()],
            ))
            .await?;
        ensure!(!fields.is_empty() && fields.len() <= 128);
        let columns = fields
            .iter()
            .map(|r| r.try_get::<String>("", "name"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let identifiers = columns
            .iter()
            .map(|c| identifier(c))
            .collect::<Result<Vec<_>>>()?;
        let length = identifiers
            .iter()
            .map(|c| format!("coalesce(length(CAST({c} AS BLOB)),0)"))
            .collect::<Vec<_>>()
            .join("+");
        let bounds = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT count(*) AS n,coalesce(sum({length}),0) AS bytes FROM {quoted}"),
            ))
            .await?
            .context("snapshot bounds")?;
        let n = usize::try_from(bounds.try_get::<i64>("", "n")?)?;
        let size = usize::try_from(bounds.try_get::<i64>("", "bytes")?)?;
        count = count.checked_add(n).context("snapshot row overflow")?;
        // quote() can double embedded delimiters. Preflight before fetching any
        // payload; these are small real fixtures, never an unbounded DB dump.
        bytes = bytes
            .checked_add(size.checked_mul(2).context("snapshot byte overflow")?)
            .and_then(|v| v.checked_add(definition.len()))
            .context("snapshot byte overflow")?;
        ensure!(count <= 4096 && bytes <= MAX);
        let projection = identifiers
            .iter()
            .enumerate()
            .map(|(i, c)| format!("quote({c}) AS c{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let order = identifiers
            .iter()
            .map(|c| format!("quote({c}) COLLATE BINARY"))
            .collect::<Vec<_>>()
            .join(",");
        let raw = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT {projection} FROM {quoted} ORDER BY {order} LIMIT 4097"),
            ))
            .await?;
        ensure!(raw.len() == n);
        let mut rows = Vec::with_capacity(raw.len());
        for row in raw {
            let mut values = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                values.push(row.try_get::<String>("", &format!("c{i}"))?);
            }
            rows.push(values);
        }
        ensure!(
            output
                .insert(
                    name,
                    Table {
                        definition,
                        columns,
                        rows
                    }
                )
                .is_none()
        );
    }
    let result = Snapshot {
        schema,
        tables: output,
    };
    ensure!(serde_json::to_vec(&result)?.len() <= MAX);
    Ok(result)
}
pub(super) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}
pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = std::fs::File::open(path)?;
    ensure!(file.metadata()?.len() <= MAX as u64);
    let mut bytes = Vec::new();
    file.take(MAX as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX);
    Ok(serde_json::from_slice(&bytes)?)
}
fn file_digest(path: &Path) -> Result<(u64, String)> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    ensure!(size <= MAX as u64);
    let mut input = file.take(MAX as u64 + 1);
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += u64::try_from(n)?;
        ensure!(bytes <= MAX as u64);
        hash.update(&buffer[..n]);
    }
    ensure!(bytes == size);
    Ok((size, hex::encode(hash.finalize())))
}
pub(super) fn archive(root: &Path) -> Result<BTreeMap<String, (u64, String)>> {
    let directory = root.join("original-files");
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let mut total = 0u64;
    for name in [
        "native-paid.db",
        "native-paid.db-wal",
        "native-paid.db-shm",
        "accepted.json",
        "answer.json",
    ] {
        let path = root.join(name);
        if !path.exists() {
            ensure!(matches!(name, "native-paid.db-wal" | "native-paid.db-shm"));
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(metadata.is_file() && metadata.len() <= MAX as u64);
        total = total
            .checked_add(metadata.len())
            .context("archive overflow")?;
        ensure!(total <= MAX as u64);
        let source = std::fs::File::open(&path)?;
        let mut target = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(directory.join(name))?;
        ensure!(std::io::copy(&mut source.take(MAX as u64 + 1), &mut target)? == metadata.len());
        target.sync_all()?;
        ensure!(file_digest(&path)? == file_digest(&directory.join(name))?);
    }
    archive_digest(root)
}
pub(super) fn archive_digest(root: &Path) -> Result<BTreeMap<String, (u64, String)>> {
    let mut output = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("original-files"))? {
        let entry = entry?;
        ensure!(entry.file_type()?.is_file());
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("archive name"))?;
        ensure!(output.len() < 5);
        output.insert(name, file_digest(&entry.path())?);
    }
    ensure!(
        output.contains_key("native-paid.db")
            && output.contains_key("accepted.json")
            && output.contains_key("answer.json")
    );
    Ok(output)
}
