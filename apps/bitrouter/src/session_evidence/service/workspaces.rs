//! Bind filesystem capture exclusions to the effective native profile.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

use super::*;

impl ControllerEvidence {
    pub(super) fn exclusions_for(&self, root: &NativeRoot) -> BTreeSet<PathBuf> {
        let mut exclusions = self.workspace_exclusions.clone();
        if let Some(native_home) = root.directory.parent() {
            exclusions.insert(native_home.to_path_buf());
        }
        exclusions
    }
}

pub(super) async fn runtime_exclusions(
    db: &DatabaseConnection,
    home: &Path,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::from([canonical_future_path(home)?]);
    if db.get_database_backend() == DbBackend::Sqlite {
        // Ask the connected engine for actual filenames, including URI decoding
        // and memory databases, instead of reimplementing SQLx URL parsing.
        // https://www.sqlite.org/pragma.html#pragma_database_list
        // https://www.sqlite.org/tempfiles.html
        for row in db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA database_list",
            ))
            .await?
        {
            let filename: String = row.try_get("", "file")?;
            if filename.is_empty() {
                continue;
            }
            let path = canonical_future_path(Path::new(&filename))?;
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let mut filename = path.as_os_str().to_os_string();
                filename.push(suffix);
                paths.insert(PathBuf::from(filename));
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests;
