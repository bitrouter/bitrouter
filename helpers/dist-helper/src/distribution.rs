//! Distinct profiled archives, built from Cargo's selected executable and OUT_DIR.

use std::{
    fs::{self, File},
    io::{self, BufRead, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use clap::{Args, ValueEnum};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zip::{ZipWriter, write::SimpleFileOptions};

#[cfg(test)]
mod tests;

#[derive(Debug, Args)]
pub(super) struct PackageOptions {
    /// Application capabilities to package.
    #[arg(long, value_enum, default_value = "gateway")]
    profile: PackageProfile,
    /// Explicit Rust target triple; custom target JSON paths are unsupported.
    #[arg(long)]
    target: String,
    /// Fresh output directory, relative to the workspace unless absolute.
    #[arg(long, default_value = "target/profile-artifacts")]
    out: PathBuf,
    /// Cargo compilation profile (independent of application capabilities).
    #[arg(long, value_enum, default_value = "dist")]
    cargo_profile: CargoProfile,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PackageProfile {
    Gateway,
    Full,
}

impl PackageProfile {
    fn identity(self) -> DistributionProfile {
        match self {
            Self::Gateway => DistributionProfile::Gateway,
            Self::Full => DistributionProfile::Full,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CargoProfile {
    Dev,
    Dist,
}

impl CargoProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Dist => "dist",
        }
    }
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
}

struct BuildArtifacts {
    executable: PathBuf,
    out_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    identity: ArtifactIdentity,
    cargo_profile: String,
    binary: Binary,
}

#[derive(Debug, Serialize, Deserialize)]
struct Binary {
    /// Relative to the archive root.
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Serialize)]
struct Summary {
    archive: PathBuf,
    checksum: PathBuf,
}

pub(super) fn run(root: &Path, options: &PackageOptions) -> Result<()> {
    let out = output_path(root, options)?;
    ensure_absent(&out)?;
    let output = cargo(root)
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .output()
        .context("running Cargo metadata")?;
    ensure!(
        output.status.success(),
        "Cargo metadata failed: {}",
        output.status
    );
    let package = select_package(&output.stdout)?;
    // Cargo releases its lock before this process can copy the executable.
    // Keep both the executable and OUT_DIR in a private build directory until
    // archive publication finishes, so another profile cannot replace either.
    let build = tempfile::Builder::new()
        .prefix("bitrouter-package-build-")
        .tempdir()
        .context("creating isolated package build directory")?;
    let output = build_command(root, options, build.path())
        .output()
        .context("building the selected application")?;
    ensure!(
        output.status.success(),
        "Cargo build failed: {}",
        output.status
    );
    let artifacts = parse_build(&output.stdout, &package.id)?;
    let summary = package_archive(root, options, &package.version, &artifacts)?;
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &summary).context("writing package summary")?;
    writeln!(stdout).context("finishing package summary")?;
    Ok(())
}

fn cargo(root: &Path) -> Command {
    let executable = match std::env::var_os("CARGO") {
        Some(value) => value,
        None => "cargo".into(),
    };
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

fn build_command(root: &Path, options: &PackageOptions, build_dir: &Path) -> Command {
    let mut command = cargo(root);
    command.args([
        "build",
        "-p",
        "bitrouter",
        "--bin",
        "bro",
        "--target",
        &options.target,
        "--profile",
        options.cargo_profile.as_str(),
        "--message-format=json-render-diagnostics",
        "--locked",
    ]);
    command.arg("--target-dir").arg(build_dir);
    if matches!(options.profile, PackageProfile::Gateway) {
        command.arg("--no-default-features");
    }
    command
}

fn select_package(bytes: &[u8]) -> Result<Package> {
    let metadata: Metadata = serde_json::from_slice(bytes).context("reading Cargo metadata")?;
    let mut packages = metadata.packages.into_iter().filter(|package| {
        package.name == "bitrouter" && metadata.workspace_members.contains(&package.id)
    });
    let package = packages
        .next()
        .context("no bitrouter workspace package in Cargo metadata")?;
    ensure!(
        packages.next().is_none(),
        "ambiguous bitrouter workspace package"
    );
    ensure!(
        !package.id.is_empty() && !package.version.is_empty(),
        "incomplete package metadata"
    );
    Ok(package)
}

fn unique_path(slot: &mut Option<PathBuf>, value: &Value, field: &str) -> Result<()> {
    let path = PathBuf::from(
        value
            .as_str()
            .with_context(|| format!("missing Cargo {field}"))?,
    );
    ensure!(path.is_absolute(), "Cargo {field} must be absolute");
    if let Some(previous) = slot {
        ensure!(*previous == path, "ambiguous Cargo {field}");
    } else {
        *slot = Some(path);
    }
    Ok(())
}

// Cargo JSON contract: exact package_id, compiler-artifact.executable and
// build-script-executed.out_dir from this invocation; never scan target/.
// https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages
fn parse_build(bytes: &[u8], package_id: &str) -> Result<BuildArtifacts> {
    let mut executable = None;
    let mut out_dir = None;
    let mut finished = false;
    for line in bytes.lines() {
        let line = line.context("reading Cargo output")?;
        if !line.starts_with('{') {
            continue;
        }
        let message: Value = serde_json::from_str(&line).context("reading Cargo JSON message")?;
        match message["reason"].as_str() {
            Some("build-finished") => {
                ensure!(
                    message["success"] == true,
                    "Cargo reported an unsuccessful build"
                );
                finished = true;
            }
            Some("compiler-artifact") if message["package_id"] == package_id => {
                if message["target"]["name"] == "bro"
                    && message["target"]["kind"] == serde_json::json!(["bin"])
                    && message["profile"]["test"] == false
                {
                    unique_path(&mut executable, &message["executable"], "executable")?;
                }
            }
            Some("build-script-executed") if message["package_id"] == package_id => {
                unique_path(&mut out_dir, &message["out_dir"], "out_dir")?;
            }
            _ => {}
        }
    }
    ensure!(finished, "missing successful Cargo build-finished message");
    Ok(BuildArtifacts {
        executable: executable.context("Cargo did not report the bro executable")?,
        out_dir: out_dir.context("Cargo did not report bitrouter's OUT_DIR")?,
    })
}

fn output_path(root: &Path, options: &PackageOptions) -> Result<PathBuf> {
    let target = &options.target;
    ensure!(
        target.len() <= 128
            && target.contains('-')
            && !target.contains("..")
            && target.split('-').all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && !part.ends_with('.')
                    && part.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'.')
                    })
            }),
        "unsafe target triple: {target}"
    );
    ensure!(
        !options.out.as_os_str().is_empty(),
        "output directory must not be empty"
    );
    for component in options.out.components() {
        match component {
            Component::ParentDir => bail!("output directory must not contain '..'"),
            Component::Prefix(prefix) => {
                ensure!(
                    options.out.is_absolute(),
                    "drive-relative output paths are unsupported"
                );
                ensure!(
                    !matches!(
                        prefix.kind(),
                        std::path::Prefix::DeviceNS(_) | std::path::Prefix::Verbatim(_)
                    ),
                    "device namespace output paths are unsupported"
                );
            }
            Component::Normal(name) => {
                let name = name.to_str().context("output path must be UTF-8")?;
                ensure!(
                    !name.ends_with(['.', ' '])
                        && !name
                            .chars()
                            .any(|c| c.is_control() || "\\:<>\"|?*".contains(c)),
                    "unsafe output path component: {name}"
                );
                let stem = name
                    .split('.')
                    .next()
                    .context("empty output component")?
                    .to_ascii_uppercase();
                ensure!(
                    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                        && !(stem.len() == 4
                            && (stem.starts_with("COM") || stem.starts_with("LPT"))
                            && matches!(stem.as_bytes()[3], b'1'..=b'9')),
                    "reserved output path component: {name}"
                );
            }
            _ => {}
        }
    }
    Ok(root.join(&options.out))
}

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "output already exists: {}; choose a fresh --out directory",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

fn validate_identity(
    identity: &ArtifactIdentity,
    options: &PackageOptions,
    version: &str,
) -> Result<()> {
    ensure!(
        identity.profile == options.profile.identity(),
        "artifact profile does not match selection"
    );
    ensure!(
        identity.target == options.target,
        "artifact target does not match selection"
    );
    ensure!(
        identity.version == version,
        "artifact version does not match Cargo metadata"
    );
    ensure!(
        identity
            .source_revision
            .as_deref()
            .is_some_and(|revision| !revision.trim().is_empty()),
        "artifact source revision must be present and nonempty"
    );
    Ok(())
}

fn package_archive(
    root: &Path,
    options: &PackageOptions,
    version: &str,
    artifacts: &BuildArtifacts,
) -> Result<Summary> {
    let out = output_path(root, options)?;
    ensure_absent(&out)?;
    let identity: ArtifactIdentity = serde_json::from_slice(
        &fs::read(artifacts.out_dir.join("artifact.json"))
            .context("reading selected artifact.json")?,
    )
    .context("parsing selected artifact identity")?;
    validate_identity(&identity, options, version)?;
    ensure!(
        fs::symlink_metadata(&artifacts.executable)
            .context("checking Cargo executable")?
            .is_file(),
        "Cargo executable must be a regular file"
    );
    let windows = options.target.split('-').any(|part| part == "windows");
    let binary_name = if windows { "bro.exe" } else { "bro" };
    ensure!(
        artifacts
            .executable
            .file_name()
            .is_some_and(|name| name == binary_name),
        "Cargo executable has an unexpected filename"
    );
    let profile = identity.profile.as_str();
    let archive_root = format!("bitrouter-{profile}-{}", options.target);
    let extension = if windows { "zip" } else { "tar.gz" };
    let archive_name = format!("{archive_root}.{extension}");
    let checksum_name = format!("{archive_name}.sha256");
    let parent = out.parent().context("output directory has no parent")?;
    fs::create_dir_all(parent).context("creating output parent directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".bitrouter-package-")
        .tempdir_in(parent)
        .context("creating package staging directory")?;
    let contents = staging.path().join(&archive_root);
    fs::create_dir(&contents).context("creating archive root")?;
    let copied_binary = contents.join(binary_name);
    fs::copy(&artifacts.executable, &copied_binary).context("copying Cargo executable")?;
    let (bytes, sha256) = hash_file(&copied_binary)?;
    ensure!(bytes > 0, "Cargo executable is empty");
    fs::copy(root.join("LICENSE"), contents.join("LICENSE")).context("copying LICENSE")?;
    let invocation = if windows { ".\\bro.exe" } else { "./bro" };
    fs::write(
        contents.join("README.md"),
        format!(
            "# BitRouter {profile}\n\nTarget: `{}`. Cargo profile: `{}`.\n\n\
         This is a distinct profiled artifact for manual installation and use.\n\
         Extract the archive on the matching target, then run `{invocation} --help`.\n\
         The gateway profile disables default application features; full uses defaults.\n\
         See manifest.json for build identity and the binary SHA-256.\n\
         These archives have their own names and layout, independent of cargo-dist releases.\n",
            options.target,
            options.cargo_profile.as_str()
        ),
    )
    .context("writing package README")?;
    let manifest = Manifest {
        identity,
        cargo_profile: options.cargo_profile.as_str().to_owned(),
        binary: Binary {
            path: binary_name.to_owned(),
            bytes,
            sha256,
        },
    };
    fs::write(
        contents.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .context("writing package manifest")?;
    let entries = [
        (binary_name, 0o755),
        ("LICENSE", 0o644),
        ("README.md", 0o644),
        ("manifest.json", 0o644),
    ];
    let archive = staging.path().join(&archive_name);
    write_archive(&archive, &contents, &archive_root, &entries, windows)?;
    let (_, digest) = hash_file(&archive)?;
    let checksum = staging.path().join(&checksum_name);
    let mut file = File::create(&checksum).context("creating archive checksum")?;
    writeln!(file, "{digest}  {archive_name}").context("writing archive checksum")?;
    file.sync_all().context("syncing archive checksum")?;
    drop(file);
    publish(&out, &archive, &checksum)?;
    Ok(Summary {
        archive: out.join(archive_name),
        checksum: out.join(checksum_name),
    })
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer).context("reading file for hashing")?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
        bytes += length as u64;
    }
    Ok((bytes, hex::encode(hash.finalize())))
}

fn write_archive(
    path: &Path,
    contents: &Path,
    root: &str,
    entries: &[(&str, u32)],
    windows: bool,
) -> Result<()> {
    let file = File::create(path).context("creating staged archive")?;
    if windows {
        let mut archive = ZipWriter::new(file);
        archive.add_directory(
            format!("{root}/"),
            SimpleFileOptions::default().unix_permissions(0o755),
        )?;
        for (name, mode) in entries {
            let mut source = File::open(contents.join(name))?;
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(*mode)
                .large_file(source.metadata()?.len() >= u32::MAX as u64);
            archive.start_file(format!("{root}/{name}"), options)?;
            io::copy(&mut source, &mut archive)?;
        }
        archive.finish()?.sync_all()?;
    } else {
        let mut archive = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        let mut directory = tar::Header::new_gnu();
        directory.set_entry_type(tar::EntryType::Directory);
        directory.set_size(0);
        directory.set_mode(0o755);
        directory.set_uid(0);
        directory.set_gid(0);
        directory.set_mtime(0);
        directory.set_cksum();
        archive.append_data(&mut directory, root, io::empty())?;
        for (name, mode) in entries {
            let mut source = File::open(contents.join(name))?;
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(source.metadata()?.len());
            header.set_mode(*mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            archive.append_data(&mut header, format!("{root}/{name}"), &mut source)?;
        }
        archive.into_inner()?.finish()?.sync_all()?;
    }
    Ok(())
}

fn publish(out: &Path, archive: &Path, checksum: &Path) -> Result<()> {
    // Reserve a fresh directory atomically, then publish only completed files.
    // Hard links on the same filesystem never overwrite another writer's file.
    // The checksum is published last; failures roll back only our own links.
    fs::create_dir(out)
        .with_context(|| format!("reserving fresh output directory {}", out.display()))?;
    let mut promoted = Vec::new();
    let result = (|| -> Result<()> {
        for source in [archive, checksum] {
            let destination = out.join(source.file_name().context("missing staged filename")?);
            fs::hard_link(source, &destination).context("promoting completed package file")?;
            promoted.push(destination);
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in promoted {
            fs::remove_file(path)
                .with_context(|| format!("rolling back package after: {error:#}"))?;
        }
        fs::remove_dir(out)
            .with_context(|| format!("removing reserved directory after: {error:#}"))?;
        return Err(error);
    }
    Ok(())
}
