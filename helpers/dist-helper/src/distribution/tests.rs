use std::collections::BTreeMap;

use clap::Parser;
use flate2::read::GzDecoder;
use serde_json::json;
use tempfile::TempDir;

use super::*;

const VERSION: &str = "1.2.3-test";
const PACKAGE_ID: &str = "path+file:///workspace/apps/bitrouter#1.2.3-test";
// Known SHA-256 vector; fake executable bytes are confined to helper unit tests.
const BINARY: &[u8] = b"abc";
const BINARY_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

struct Fixture {
    directory: TempDir,
    options: PackageOptions,
    artifacts: BuildArtifacts,
    identity: ArtifactIdentity,
}

impl Fixture {
    fn new(target: &str, profile: PackageProfile) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let out_dir = root.join("selected-out");
        fs::create_dir(&out_dir)?;
        let name = if target.contains("-windows-") {
            "bro.exe"
        } else {
            "bro"
        };
        let executable = root.join(name);
        fs::write(&executable, BINARY)?;
        fs::write(root.join("LICENSE"), b"test license\n")?;
        let identity = ArtifactIdentity {
            version: VERSION.to_owned(),
            source_revision: Some("0123456789abcdef".to_owned()),
            target: target.to_owned(),
            features: Vec::new(),
            profile: profile.identity(),
            handoff_build_id: "test-build-id".to_owned(),
        };
        fs::write(
            out_dir.join("artifact.json"),
            serde_json::to_vec(&identity)?,
        )?;
        // A tempting stale artifact must never be discovered by directory scans.
        fs::create_dir(root.join("stale-out"))?;
        fs::write(
            root.join("stale-out/artifact.json"),
            b"not selected metadata",
        )?;
        Ok(Self {
            directory,
            options: PackageOptions {
                profile,
                target: target.to_owned(),
                out: PathBuf::from("packages"),
                cargo_profile: CargoProfile::Dev,
            },
            artifacts: BuildArtifacts {
                executable,
                out_dir,
            },
            identity,
        })
    }

    fn package(&self) -> Result<Summary> {
        package_archive(
            self.directory.path(),
            &self.options,
            VERSION,
            &self.artifacts,
        )
    }

    fn out(&self) -> PathBuf {
        self.directory.path().join(&self.options.out)
    }

    fn check_no_staging(&self) -> Result<()> {
        for entry in fs::read_dir(self.directory.path())? {
            ensure!(
                !entry?
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bitrouter-package-"),
                "staging directory leaked"
            );
        }
        Ok(())
    }
}

#[test]
fn cli_requires_target_and_limits_profile_choices() -> Result<()> {
    ensure!(crate::Cli::try_parse_from(["dist-helper", "package"]).is_err());
    let cli = crate::Cli::try_parse_from([
        "dist-helper",
        "package",
        "--target",
        "x86_64-unknown-linux-gnu",
    ])?;
    let crate::Command::Package(options) = cli.command else {
        bail!("wrong subcommand")
    };
    ensure!(matches!(options.profile, PackageProfile::Gateway));
    ensure!(matches!(options.cargo_profile, CargoProfile::Dist));
    ensure!(options.out == Path::new("target/profile-artifacts"));
    let explicit = crate::Cli::try_parse_from([
        "dist-helper",
        "package",
        "--profile",
        "gateway",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--cargo-profile",
        "dev",
        "--out",
        "fresh-package",
    ])?;
    let crate::Command::Package(options) = explicit.command else {
        bail!("wrong subcommand")
    };
    ensure!(matches!(options.profile, PackageProfile::Gateway));
    ensure!(matches!(options.cargo_profile, CargoProfile::Dev));
    ensure!(options.out == Path::new("fresh-package"));
    for extra in [
        ["--profile", "custom"],
        ["--cargo-profile", "release"],
        ["--force", "true"],
    ] {
        ensure!(
            crate::Cli::try_parse_from([
                "dist-helper",
                "package",
                "--target",
                "x86_64-unknown-linux-gnu",
                extra[0],
                extra[1]
            ])
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn cargo_build_selects_only_the_requested_binary_and_features() -> Result<()> {
    let mut fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    for (profile, cargo_profile) in [
        (PackageProfile::Gateway, CargoProfile::Dist),
        (PackageProfile::Full, CargoProfile::Dev),
    ] {
        fixture.options.profile = profile;
        fixture.options.cargo_profile = cargo_profile;
        let build_dir = fixture.directory.path().join("isolated-build");
        let command = build_command(fixture.directory.path(), &fixture.options, &build_dir);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut expected = vec![
            "build",
            "-p",
            "bitrouter",
            "--bin",
            "bro",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--profile",
            cargo_profile.as_str(),
            "--message-format=json-render-diagnostics",
            "--locked",
            "--target-dir",
            build_dir.to_str().context("test path must be UTF-8")?,
        ];
        if matches!(profile, PackageProfile::Gateway) {
            expected.push("--no-default-features");
        }
        ensure!(args == expected);
        ensure!(command.get_current_dir() == Some(fixture.directory.path()));
    }
    Ok(())
}

#[test]
fn metadata_selects_exact_workspace_package_id() -> Result<()> {
    let metadata = json!({
        "workspace_members": [PACKAGE_ID],
        "packages": [
            {"id": "registry+https://example.invalid#bitrouter@1.2.3-test", "name": "bitrouter", "version": VERSION},
            {"id": PACKAGE_ID, "name": "bitrouter", "version": VERSION}
        ]
    });
    let selected = select_package(&serde_json::to_vec(&metadata)?)?;
    ensure!(selected.id == PACKAGE_ID && selected.version == VERSION);
    let mut missing = metadata.clone();
    missing["workspace_members"] = json!([]);
    ensure!(select_package(&serde_json::to_vec(&missing)?).is_err());
    let mut ambiguous = metadata;
    ambiguous["packages"]
        .as_array_mut()
        .context("packages array")?
        .push(json!({"id": PACKAGE_ID, "name": "bitrouter", "version": VERSION}));
    ensure!(select_package(&serde_json::to_vec(&ambiguous)?).is_err());
    Ok(())
}

fn messages(fixture: &Fixture) -> Vec<Value> {
    vec![
        json!({"reason": "compiler-artifact", "package_id": "other", "target": {"name": "bro", "kind": ["bin"]},
            "profile": {"test": false}, "executable": fixture.directory.path().join("wrong-bro")}),
        json!({"reason": "build-script-executed", "package_id": "other", "out_dir": fixture.directory.path().join("stale-out")}),
        json!({"reason": "compiler-artifact", "package_id": PACKAGE_ID, "target": {"name": "build-script-build", "kind": ["custom-build"]},
            "executable": fixture.directory.path().join("build-script")}),
        json!({"reason": "compiler-artifact", "package_id": PACKAGE_ID, "target": {"name": "bro", "kind": ["bin"]},
            "profile": {"test": true}, "executable": fixture.directory.path().join("test-bro")}),
        json!({"reason": "compiler-artifact", "package_id": PACKAGE_ID, "target": {"name": "bro", "kind": ["bin"]},
            "profile": {"test": false}, "fresh": true, "executable": fixture.artifacts.executable}),
        json!({"reason": "build-script-executed", "package_id": PACKAGE_ID, "out_dir": fixture.artifacts.out_dir}),
        json!({"reason": "build-finished", "success": true}),
    ]
}

fn log(messages: &[Value]) -> Result<Vec<u8>> {
    let mut output = b"non-JSON tool output\n".to_vec();
    for message in messages {
        serde_json::to_writer(&mut output, message)?;
        output.push(b'\n');
    }
    Ok(output)
}

#[test]
fn cargo_messages_pair_only_the_selected_executable_and_out_dir() -> Result<()> {
    let fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    let artifacts = parse_build(&log(&messages(&fixture))?, PACKAGE_ID)?;
    ensure!(artifacts.executable == fixture.artifacts.executable);
    ensure!(artifacts.out_dir == fixture.artifacts.out_dir);
    ensure!(parse_build(&log(&messages(&fixture))?, "wrong-package-id").is_err());
    let summary = package_archive(
        fixture.directory.path(),
        &fixture.options,
        VERSION,
        &artifacts,
    )?;
    ensure!(summary.archive.is_file());
    Ok(())
}

#[test]
fn incomplete_failed_or_ambiguous_cargo_messages_are_rejected() -> Result<()> {
    let fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    let original = messages(&fixture);
    for removed in [4, 5, 6] {
        let mut incomplete = original.clone();
        incomplete.remove(removed);
        ensure!(parse_build(&log(&incomplete)?, PACKAGE_ID).is_err());
    }
    for extra in [
        json!({"reason": "build-finished", "success": false}),
        json!({"reason": "build-script-executed", "package_id": PACKAGE_ID, "out_dir": fixture.directory.path().join("stale-out")}),
        json!({"reason": "build-script-executed", "package_id": PACKAGE_ID, "out_dir": "relative/path"}),
        json!({"reason": "compiler-artifact", "package_id": PACKAGE_ID, "target": {"name": "bro", "kind": ["bin"]},
            "profile": {"test": false}, "executable": fixture.directory.path().join("other-bro")}),
    ] {
        let mut invalid = original.clone();
        invalid.push(extra);
        ensure!(parse_build(&log(&invalid)?, PACKAGE_ID).is_err());
    }
    ensure!(parse_build(b"{malformed JSON}\n", PACKAGE_ID).is_err());
    Ok(())
}

fn archive_contents(
    path: &Path,
    root: &str,
    windows: bool,
) -> Result<BTreeMap<String, (Vec<u8>, u32)>> {
    let mut files = BTreeMap::new();
    let mut directories = 0;
    if windows {
        let mut archive = zip::ZipArchive::new(File::open(path)?)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let mode = entry.unix_mode().context("missing ZIP mode")? & 0o777;
            if entry.is_dir() {
                ensure!(entry.name()? == format!("{root}/") && mode == 0o755);
                directories += 1;
                continue;
            }
            let name = entry
                .name()?
                .strip_prefix(&format!("{root}/"))
                .context("wrong ZIP root")?
                .to_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            ensure!(
                files.insert(name, (bytes, mode)).is_none(),
                "duplicate ZIP entry"
            );
        }
    } else {
        let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let mode = entry.header().mode()? & 0o777;
            if entry.header().entry_type().is_dir() {
                ensure!(entry.path()?.as_ref() == Path::new(root) && mode == 0o755);
                directories += 1;
                continue;
            }
            ensure!(entry.header().entry_type().is_file());
            let name = entry
                .path()?
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF-8 TAR path")?
                .to_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            ensure!(
                files.insert(name, (bytes, mode)).is_none(),
                "duplicate TAR entry"
            );
        }
    }
    ensure!(directories == 1 && files.len() == 4);
    Ok(files)
}

#[test]
fn profiled_archives_round_trip_contents_modes_identity_and_hashes() -> Result<()> {
    for target in ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"] {
        for profile in [PackageProfile::Gateway, PackageProfile::Full] {
            let fixture = Fixture::new(target, profile)?;
            let windows = target.contains("-windows-");
            let root = format!("bitrouter-{}-{target}", profile.identity().as_str());
            let extension = if windows { "zip" } else { "tar.gz" };
            let filename = format!("{root}.{extension}");
            let summary = fixture.package()?;
            ensure!(summary.archive == fixture.out().join(&filename));
            ensure!(summary.checksum == fixture.out().join(format!("{filename}.sha256")));
            let files = archive_contents(&summary.archive, &root, windows)?;
            let binary_name = if windows { "bro.exe" } else { "bro" };
            let (binary, mode) = files.get(binary_name).context("missing binary")?;
            ensure!(binary == BINARY && *mode == 0o755);
            ensure!(files.get("LICENSE") == Some(&(b"test license\n".to_vec(), 0o644)));
            let (readme, mode) = files.get("README.md").context("missing README")?;
            let readme = std::str::from_utf8(readme)?;
            ensure!(
                *mode == 0o644
                    && readme.contains("manual")
                    && readme.contains(profile.identity().as_str())
            );
            let (bytes, mode) = files.get("manifest.json").context("missing manifest")?;
            let manifest: Manifest = serde_json::from_slice(bytes)?;
            ensure!(*mode == 0o644 && manifest.identity == fixture.identity);
            ensure!(manifest.cargo_profile == "dev" && manifest.binary.path == binary_name);
            ensure!(manifest.binary.bytes == 3 && manifest.binary.sha256 == BINARY_SHA256);
            let expected_hash = hex::encode(Sha256::digest(fs::read(&summary.archive)?));
            ensure!(
                fs::read_to_string(&summary.checksum)? == format!("{expected_hash}  {filename}\n")
            );
            ensure!(fs::read_dir(fixture.out())?.count() == 2);
            fixture.check_no_staging()?;
        }
    }
    Ok(())
}

#[test]
fn wrong_identity_or_missing_revision_never_produces_outputs() -> Result<()> {
    let fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    for (field, value) in [
        ("profile", json!("full")),
        ("profile", json!("custom")),
        ("target", json!("aarch64-apple-darwin")),
        ("version", json!("0.0.0")),
        ("source_revision", Value::Null),
        ("source_revision", json!("")),
        ("source_revision", json!(" \n\t")),
    ] {
        let mut identity = serde_json::to_value(&fixture.identity)?;
        identity[field] = value;
        fs::write(
            fixture.artifacts.out_dir.join("artifact.json"),
            serde_json::to_vec(&identity)?,
        )?;
        ensure!(fixture.package().is_err(), "accepted incorrect {field}");
        ensure!(!fixture.out().try_exists()?);
        fixture.check_no_staging()?;
    }
    Ok(())
}

#[test]
fn existing_output_directories_and_files_are_preserved() -> Result<()> {
    let fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    fs::create_dir(fixture.out())?;
    ensure!(fixture.package().is_err());
    ensure!(fixture.out().is_dir() && fs::read_dir(fixture.out())?.count() == 0);
    let previous = fixture
        .out()
        .join("bitrouter-gateway-x86_64-unknown-linux-gnu.tar.gz");
    fs::write(&previous, b"previous archive")?;
    ensure!(fixture.package().is_err());
    ensure!(fs::read(&previous)? == b"previous archive");
    fs::remove_file(previous)?;
    fs::remove_dir(fixture.out())?;
    fs::write(fixture.out(), b"previous output file")?;
    ensure!(fixture.package().is_err());
    ensure!(fs::read(fixture.out())? == b"previous output file");
    fixture.check_no_staging()
}

#[cfg(unix)]
#[test]
fn dangling_output_symlink_is_preserved() -> Result<()> {
    let fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    let missing = fixture.directory.path().join("missing");
    std::os::unix::fs::symlink(&missing, fixture.out())?;
    ensure!(fixture.package().is_err());
    ensure!(fs::read_link(fixture.out())? == missing);
    fixture.check_no_staging()
}

#[test]
fn staging_and_promotion_failures_leave_no_partial_package() -> Result<()> {
    let fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    fs::remove_file(fixture.directory.path().join("LICENSE"))?;
    ensure!(fixture.package().is_err());
    ensure!(!fixture.out().try_exists()?);
    fixture.check_no_staging()?;
    let missing_checksum = fixture.directory.path().join("missing.sha256");
    ensure!(
        publish(
            &fixture.out(),
            &fixture.artifacts.executable,
            &missing_checksum
        )
        .is_err()
    );
    ensure!(!fixture.out().try_exists()?);
    ensure!(fs::read(&fixture.artifacts.executable)? == BINARY);
    Ok(())
}

#[test]
fn unsafe_target_and_output_components_are_rejected() -> Result<()> {
    let mut fixture = Fixture::new("x86_64-unknown-linux-gnu", PackageProfile::Gateway)?;
    for target in [
        "",
        "../target",
        "--target",
        "x86_64/evil",
        "x86_64\\evil",
        "x86_64-..-linux",
        "x86_64-pc-windows\n",
    ] {
        fixture.options.target = target.to_owned();
        ensure!(
            output_path(fixture.directory.path(), &fixture.options).is_err(),
            "accepted {target:?}"
        );
    }
    fixture.options.target = "thumbv8m.main-none-eabi".to_owned();
    ensure!(output_path(fixture.directory.path(), &fixture.options).is_ok());
    for out in [
        "",
        "../escape",
        "some/../escape",
        "bad\nname",
        "bad:name",
        "NUL",
        "COM1.txt",
        "trailing.",
    ] {
        fixture.options.out = PathBuf::from(out);
        ensure!(
            output_path(fixture.directory.path(), &fixture.options).is_err(),
            "accepted {out:?}"
        );
    }
    #[cfg(not(windows))]
    {
        fixture.options.out = PathBuf::from("some\\escape");
        ensure!(output_path(fixture.directory.path(), &fixture.options).is_err());
    }
    Ok(())
}
