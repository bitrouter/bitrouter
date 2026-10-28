//! Secured, bounded bundle I/O. Source database descriptors belong only to SQLite.
use super::format::{MAX_EVIDENCE, MAX_FILE};
use anyhow::{Result, ensure};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

pub(super) struct Fingerprint(Hmac<Sha256>);
impl Fingerprint {
    pub fn new(key: &[u8; 32], domain: &[u8]) -> Result<Self> {
        // Independent evidence domain; never a credential or runtime wrapping key.
        // https://www.rfc-editor.org/rfc/rfc2104
        let mac = Hmac::<Sha256>::new_from_slice(key)
            .map_err(|_| anyhow::anyhow!("offline fingerprint key invalid"))?;
        let mut value = Self(mac);
        value.field(b"bitrouter.offline.sqlite-evidence.v1");
        value.field(domain);
        Ok(value)
    }
    pub fn field(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }
    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into_bytes().into()
    }
}
pub(super) fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub(super) fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
        "invalid evidence digest"
    );
    Ok(())
}
pub(super) fn create_directory(path: &Path) -> Result<()> {
    ensure!(cfg!(unix), "offline evidence is qualified only on Unix");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    private_directory(path)?;
    write_new(
        &path.join("INCOMPLETE"),
        b"Incomplete DB-only evidence; do not use for recovery.\n",
    )?;
    sync_directory(path)
}
pub(super) fn private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "bundle is not a directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Refuse symlink aliases and group/world access to a secret-bearing copy.
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "bundle directory is not private"
        );
    }
    Ok(())
}
pub(super) fn private_file(path: &Path, maximum: u64) -> Result<File> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= maximum,
        "invalid bundle file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.nlink() == 1 && metadata.permissions().mode() & 0o077 == 0,
            "bundle file is not private"
        );
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            opened.dev() == metadata.dev()
                && opened.ino() == metadata.ino()
                && opened.nlink() == 1
                && opened.permissions().mode() & 0o077 == 0,
            "bundle file changed"
        );
    }
    ensure!(opened.len() == metadata.len(), "bundle file changed");
    Ok(file)
}
pub(super) fn read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = private_file(path, maximum)?;
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "bundle file exceeds profile");
    Ok(bytes)
}
pub(super) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub(super) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub(super) fn snapshot_digest(path: &Path, key: &[u8; 32]) -> Result<(u64, String)> {
    // Only called after all SQLite handles to the copied database are closed.
    let mut file = private_file(path, MAX_FILE)?;
    let length = file.metadata()?.len();
    let mut digest = Fingerprint::new(key, b"snapshot-file")?;
    digest.field(&length.to_be_bytes());
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| anyhow::anyhow!("file length overflow"))?;
        ensure!(total <= MAX_FILE, "snapshot exceeds profile");
        // Stream update is independent of the OS read chunk sizes.
        digest.0.update(&buffer[..count]);
    }
    ensure!(total == length, "snapshot changed while hashing");
    Ok((length, hex::encode(digest.finish())))
}
pub(super) fn evidence_bytes(value: &super::format::Evidence) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_EVIDENCE,
        "evidence exceeds profile"
    );
    Ok(bytes)
}
