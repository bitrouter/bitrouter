//! Binary agent installation — download and extract platform archives.

use std::io::Read;
use std::path::{Path, PathBuf};

use bitrouter_config::BinaryArchive;
use tokio::sync::mpsc;

use super::platform::current_platform;
use super::types::InstallProgress;

/// Default install directory for downloaded agent binaries.
fn install_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME environment variable not set".to_owned())?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// Install a binary-distributed agent for the current platform.
///
/// Downloads the archive, extracts it, and places the binary in
/// `~/.local/bin/`. Sends progress updates via `progress_tx`.
///
/// Returns the absolute path to the installed binary on success.
pub async fn install_binary_agent(
    agent_name: &str,
    platforms: &std::collections::HashMap<String, BinaryArchive>,
    progress_tx: mpsc::Sender<InstallProgress>,
) -> Result<PathBuf, String> {
    let platform =
        current_platform().ok_or_else(|| "unsupported platform for binary download".to_owned())?;

    let archive_info = platforms
        .get(platform)
        .ok_or_else(|| format!("no binary available for platform {platform}"))?;

    let dest_dir = install_dir()?;
    tokio::fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| format!("failed to create install directory: {e}"))?;

    // Download archive to memory.
    let _ = progress_tx
        .send(InstallProgress::Downloading {
            bytes_received: 0,
            total: None,
        })
        .await;

    let response = reqwest::get(&archive_info.archive)
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("download failed with status {}", response.status()));
    }

    let total = response.content_length();
    let archive_bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read archive body: {e}"))?;

    let _ = progress_tx
        .send(InstallProgress::Downloading {
            bytes_received: archive_bytes.len() as u64,
            total,
        })
        .await;

    // Extract.
    let _ = progress_tx.send(InstallProgress::Extracting).await;

    let cmd_name = archive_info.cmd.trim_start_matches("./");
    let dest_binary = dest_dir.join(agent_name);

    let url = &archive_info.archive;
    if url.ends_with(".zip") {
        extract_zip(&archive_bytes, cmd_name, &dest_binary)?;
    } else {
        extract_tar_gz(&archive_bytes, cmd_name, &dest_binary)?;
    }

    // Set executable permission.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&dest_binary, perms)
            .map_err(|e| format!("failed to set executable permission: {e}"))?;
    }

    let _ = progress_tx
        .send(InstallProgress::Done(dest_binary.clone()))
        .await;
    Ok(dest_binary)
}

/// Extract a specific file from a `.tar.gz` archive in memory.
fn extract_tar_gz(data: &[u8], target_name: &str, dest: &Path) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(data);
    let mut archive = Archive::new(decoder);

    let entries = archive
        .entries()
        .map_err(|e| format!("failed to read archive entries: {e}"))?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(|e| format!("failed to read archive entry: {e}"))?;

        let path = entry
            .path()
            .map_err(|e| format!("invalid entry path: {e}"))?;

        let matches = path.file_name().map(|f| f == target_name).unwrap_or(false)
            || path.ends_with(target_name);

        if matches {
            entry
                .unpack(dest)
                .map_err(|e| format!("failed to extract binary: {e}"))?;
            return Ok(());
        }
    }

    Err(format!("binary '{target_name}' not found in archive"))
}

/// Extract a specific file from a `.zip` archive in memory.
fn extract_zip(data: &[u8], target_name: &str, dest: &Path) -> Result<(), String> {
    let reader = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("failed to open zip archive: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("failed to read zip entry: {e}"))?;

        let name = file.name().to_string();
        let matches = name == target_name
            || name.ends_with(&format!("/{target_name}"))
            || Path::new(&name)
                .file_name()
                .map(|f| f == target_name)
                .unwrap_or(false);

        if matches && !file.is_dir() {
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)
                .map_err(|e| format!("failed to read zip entry contents: {e}"))?;
            std::fs::write(dest, &contents)
                .map_err(|e| format!("failed to write extracted binary: {e}"))?;
            return Ok(());
        }
    }

    Err(format!("binary '{target_name}' not found in zip archive"))
}
