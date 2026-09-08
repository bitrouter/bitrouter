//! Resolve a maintained adapter's own dependency rather than a hoisted peer.

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(super) struct Adapter {
    pub package: &'static str,
    pub bin: &'static str,
}

pub(super) const CODEX: Adapter = Adapter {
    package: "@agentclientprotocol/codex-acp",
    bin: "codex-acp",
};
pub(super) const CLAUDE: Adapter = Adapter {
    package: "@agentclientprotocol/claude-agent-acp",
    bin: "claude-agent-acp",
};

pub(super) fn node(search: Option<std::ffi::OsString>) -> Result<PathBuf> {
    std::env::var_os("npm_node_execpath")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .map(Ok)
        .unwrap_or_else(|| {
            find_executable(
                Path::new(if cfg!(windows) { "node.exe" } else { "node" }),
                search,
            )
        })
}

/// Match the maintained adapters' createRequire(import.meta.url) boundary.
/// Rust canonical paths can carry Windows verbatim prefixes; use the file URL
/// contract instead of exposing that platform-specific spelling to Node.
/// https://nodejs.org/api/module.html#modulecreaterequirefilename
pub(super) fn module_url(entry: &Path) -> Result<url::Url> {
    url::Url::from_file_path(entry)
        .map_err(|()| anyhow::anyhow!("native adapter entry has no file URL representation"))
}

pub(super) fn adapter_entry(
    spec: Adapter,
    explicit: Option<PathBuf>,
    search: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(entry) = explicit {
        return package_entry(spec, &entry)?
            .context("configured native adapter entry is not in its package");
    }
    for directory in std::env::split_paths(search.context("adapter PATH is unavailable")?) {
        for name in [
            spec.bin.to_owned(),
            format!("{}.cmd", spec.bin),
            format!("{}.exe", spec.bin),
        ] {
            let command = directory.join(name);
            if !command.is_file() {
                continue;
            }
            if let Some(entry) = package_entry(spec, &command)? {
                return Ok(entry);
            }
            // npm's Windows/global shims and pnpm's shell shims live outside
            // the package. Resolve its bin declaration, then let Node choose
            // the dependency relative to the real package entry.
            let local = directory.parent().map(|parent| parent.join(spec.package));
            for package in [
                local,
                Some(directory.join("node_modules").join(spec.package)),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(entry) = declared_entry(spec, &package)? {
                    return Ok(entry);
                }
            }
            anyhow::bail!(
                "cannot identify the running native adapter package; configure its adapter entry override"
            );
        }
    }
    anyhow::bail!("native adapter entry is unavailable; configure its adapter entry override")
}

fn package_entry(spec: Adapter, command: &Path) -> Result<Option<PathBuf>> {
    let command = std::fs::canonicalize(command)?;
    for parent in command.ancestors().skip(1).take(8) {
        if let Some(entry) = declared_entry(spec, parent)? {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn declared_entry(spec: Adapter, package: &Path) -> Result<Option<PathBuf>> {
    let manifest = package.join("package.json");
    if !manifest.is_file() {
        return Ok(None);
    }
    ensure!(
        std::fs::metadata(&manifest)?.len() <= 1024 * 1024,
        "adapter package manifest size limit"
    );
    let manifest: Value = serde_json::from_slice(&std::fs::read(manifest)?)?;
    if manifest.get("name").and_then(Value::as_str) != Some(spec.package) {
        return Ok(None);
    }
    let bin = manifest
        .get("bin")
        .and_then(|bin| bin.get(spec.bin))
        .or_else(|| manifest.get("bin"))
        .and_then(Value::as_str)
        .context("adapter package bin is missing")?;
    Ok(Some(std::fs::canonicalize(package.join(bin))?))
}

pub(super) fn find_executable(
    name: &Path,
    search_path: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    for directory in std::env::split_paths(&search_path.context("adapter PATH is unavailable")?) {
        let candidate = directory.join(name);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(candidate);
    }
    anyhow::bail!(
        "native runtime was not found in the adapter PATH; configure its executable override"
    )
}
