//! Package-local build metadata tests; no application lifecycle is started.

#[path = "../build_artifact.rs"]
mod build_artifact;

use std::error::Error;
use std::path::Path;
use std::process::Command;

use build_artifact::{BuildMetadata, FeatureSelection, source_revision};
use sha2::{Digest, Sha256};

const MANIFEST: &str = include_str!("../Cargo.toml");

fn selection(manifest: &str, names: &[&str]) -> Result<FeatureSelection, Box<dyn Error>> {
    FeatureSelection::from_manifest(manifest, |variable| {
        names.iter().any(|name| {
            variable == format!("CARGO_FEATURE_{}", name.replace('-', "_").to_uppercase())
        })
    })
}

#[test]
fn canonical_aliases_expand_transitive_features_and_keep_exact_custom_sets()
-> Result<(), Box<dyn Error>> {
    for (roots, expected) in [
        (vec!["otel"], vec!["otel-http"]),
        (vec!["harness"], vec!["harness", "mcp"]),
        (vec!["server-tools"], vec!["mcp", "server-tools"]),
        (
            vec!["otel", "otel-http", "otel-grpc"],
            vec!["otel-grpc", "otel-http"],
        ),
        (
            vec!["harness", "core", "mcp", "harness"],
            vec!["core", "harness", "mcp"],
        ),
    ] {
        let selected = selection(MANIFEST, &roots)?;
        assert_eq!(selected.features, expected);
        assert_eq!(selected.profile, "Custom");
        // These normalized names are a reproducible --no-default-features set.
        assert_eq!(selection(MANIFEST, &expected)?.features, selected.features);
    }
    let gateway = selection(MANIFEST, &[])?;
    assert!(gateway.features.is_empty());
    assert_eq!(gateway.profile, "Gateway");
    Ok(())
}

#[test]
fn only_the_manifest_default_closure_is_full_and_all_features_are_custom()
-> Result<(), Box<dyn Error>> {
    let manifest: toml::Value = toml::from_str(MANIFEST)?;
    let features = manifest["features"]
        .as_table()
        .ok_or("missing feature table")?;
    let defaults = features["default"]
        .as_array()
        .ok_or("missing defaults")?
        .iter()
        .map(|value| value.as_str().ok_or("invalid default feature"))
        .collect::<Result<Vec<_>, _>>()?;
    let default = selection(MANIFEST, &["default"])?;
    let explicit = selection(MANIFEST, &defaults)?;
    let normalized = explicit
        .features
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(default.profile, "Full");
    assert_eq!(explicit.profile, "Full");
    assert_eq!(default.features, explicit.features);
    assert_eq!(selection(MANIFEST, &normalized)?.profile, "Full");
    let all = features.keys().map(String::as_str).collect::<Vec<_>>();
    let all = selection(MANIFEST, &all)?;
    assert_eq!(all.profile, "Custom");
    assert!(all.features.contains(&"guardrails".to_owned()));
    assert!(all.features.contains(&"otel-grpc".to_owned()));
    assert!(
        !all.features
            .iter()
            .any(|name| matches!(name.as_str(), "default" | "otel"))
    );
    Ok(())
}

#[test]
fn manifest_edits_drive_classification_without_a_hardcoded_default_list()
-> Result<(), Box<dyn Error>> {
    let fixture = r#"
        [features]
        default = ["new-capability", "otel"]
        new-capability = ["nested", "dep:implementation"]
        nested = []
        otel = ["otel-http"]
        otel-http = ["implementation/http"]
        extra = []
        [dependencies]
        implementation = { version = "1", optional = true }
        implicit = { version = "1", optional = true }
    "#;
    let full = selection(fixture, &["default"])?;
    assert_eq!(full.profile, "Full");
    assert_eq!(full.features, ["nested", "new-capability", "otel-http"]);
    assert_eq!(
        selection(fixture, &["new-capability", "otel-http"])?.profile,
        "Full"
    );
    assert_eq!(selection(fixture, &["default", "extra"])?.profile, "Custom");
    assert_eq!(selection(fixture, &["implicit"])?.features, ["implicit"]);
    let conflicting = "[features]\ndefault = []\nx-y = []\nx_y = []\n";
    assert!(selection(conflicting, &[]).is_err());
    Ok(())
}

fn metadata(features: &[&str]) -> Result<BuildMetadata, Box<dyn Error>> {
    Ok(BuildMetadata {
        version: "1.2.3".to_owned(),
        source_revision: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        target: "x86_64-unknown-linux-gnu".to_owned(),
        selection: selection(MANIFEST, features)?,
    })
}

#[test]
fn shared_gateway_has_exact_capabilities_and_a_distinct_wire_identity() -> Result<(), Box<dyn Error>>
{
    use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};

    for roots in [
        vec!["multi-instance"],
        vec!["db-postgres", "multi-instance"],
    ] {
        let selected = selection(MANIFEST, &roots)?;
        assert_eq!(selected.features, ["db-postgres", "multi-instance"]);
        assert_eq!(selected.profile, "GatewayShared");
        assert_eq!(selected.wire_profile, "gateway-shared");
        let built = metadata(&roots)?;
        let identity: ArtifactIdentity = serde_json::from_value(built.artifact_json("fixture"))?;
        assert_eq!(identity.profile, DistributionProfile::GatewayShared);
        assert_eq!(identity.features, selected.features);
        assert!(
            built
                .render("fixture")
                .contains("DistributionProfile::GatewayShared")
        );
        assert_ne!(
            fingerprint(&built, b"same sources")?,
            fingerprint(&metadata(&[])?, b"same sources")?
        );
    }
    for roots in [
        vec!["db-postgres"],
        vec!["multi-instance", "core"],
        vec!["multi-instance", "otel"],
        vec!["multi-instance", "db-mysql"],
        vec!["multi-instance", "default"],
    ] {
        assert_eq!(selection(MANIFEST, &roots)?.profile, "Custom");
    }
    Ok(())
}

#[test]
fn dependency_edges_preserve_implicit_features_without_inventing_local_capabilities()
-> Result<(), Box<dyn Error>> {
    let fixture = r#"
        [features]
        default = ["strong"]
        strong = ["implicit/codec", "ordinary/codec"]
        weak = ["implicit?/codec"]
        ordinary = []
        [dependencies.implicit]
        version = "1"
        optional = true
        [dependencies.ordinary]
        version = "1"
    "#;
    let full = selection(fixture, &["default"])?;
    assert_eq!(full.features, ["implicit", "strong"]);
    assert_eq!(full.profile, "Full");
    assert_eq!(selection(fixture, &["weak"])?.features, ["weak"]);
    // The equivalent explicit form can appear in a packaged Cargo.toml.
    let packaged = fixture.replace(
        "ordinary = []",
        "ordinary = []\nimplicit = [\"dep:implicit\"]",
    );
    assert_eq!(selection(&packaged, &["default"])?.features, full.features);
    Ok(())
}

fn fingerprint(metadata: &BuildMetadata, sources: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut hash = Sha256::new();
    hash.update(sources);
    metadata.extend_handoff_hash(&mut hash)?;
    Ok(hash.finalize().to_vec())
}

#[test]
fn handoff_identity_retains_source_inputs_and_distinguishes_build_metadata()
-> Result<(), Box<dyn Error>> {
    let sources = b"existing handoff and migration sources";
    let mut full = metadata(&["default"])?;
    let original = fingerprint(&full, sources)?;
    assert_ne!(original, fingerprint(&metadata(&[])?, sources)?);
    assert_ne!(original, fingerprint(&full, b"changed migration sources")?);
    full.target = "aarch64-apple-darwin".to_owned();
    assert_ne!(original, fingerprint(&full, sources)?);
    let mut full = metadata(&["default"])?;
    full.source_revision = Some("abcdef0123456789abcdef0123456789abcdef01".to_owned());
    assert_ne!(original, fingerprint(&full, sources)?);
    full.source_revision = None;
    assert_ne!(original, fingerprint(&full, sources)?);
    assert_eq!(
        fingerprint(&metadata(&["otel"])?, sources)?,
        fingerprint(&metadata(&["otel-http"])?, sources)?
    );
    Ok(())
}

#[test]
fn generated_identity_agrees_with_compiled_capabilities_and_legacy_handoff_id()
-> Result<(), Box<dyn Error>> {
    let identity = bitrouter::artifact::current();
    assert_eq!(identity, bitrouter::artifact::current());
    assert_eq!(identity.version, bitrouter::VERSION);
    assert_eq!(identity.handoff_build_id, bitrouter::HANDOFF_BUILD_ID);
    assert_eq!(identity.handoff_build_id.len(), 64);
    assert!(!identity.target.is_empty());
    assert!(!identity.target.contains(['/', '\\']));
    assert!(identity.features.windows(2).all(|pair| pair[0] < pair[1]));
    for (name, enabled) in [
        ("core", cfg!(feature = "core")),
        ("harness", cfg!(feature = "harness")),
        ("mcp", cfg!(feature = "mcp")),
        ("server-tools", cfg!(feature = "server-tools")),
        ("guardrails", cfg!(feature = "guardrails")),
        ("otel-http", cfg!(feature = "otel-http")),
        ("otel-grpc", cfg!(feature = "otel-grpc")),
        ("multi-instance", cfg!(feature = "multi-instance")),
        ("db-postgres", cfg!(feature = "db-postgres")),
        ("db-mysql", cfg!(feature = "db-mysql")),
    ] {
        assert_eq!(
            identity.features.iter().any(|feature| feature == name),
            enabled,
            "{name}"
        );
    }
    let names = identity
        .features
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let replay = selection(MANIFEST, &names)?;
    assert_eq!(replay.features, identity.features);
    assert_eq!(replay.wire_profile, identity.profile.as_str());
    let metadata = BuildMetadata {
        version: identity.version.clone(),
        source_revision: identity.source_revision.clone(),
        target: identity.target.clone(),
        selection: replay,
    };
    assert_eq!(
        metadata.artifact_json(&identity.handoff_build_id),
        serde_json::to_value(&identity)?
    );
    let packaged: bitrouter_ai::artifact::ArtifactIdentity =
        serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/artifact.json")))?;
    assert_eq!(packaged, identity);
    Ok(())
}

#[test]
fn explicit_revision_is_validated_and_wins_over_packaged_metadata() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join(".cargo_vcs_info.json"), "malformed")?;
    let revision = source_revision(directory.path(), Some("ABCDEF0"))?;
    assert_eq!(revision.value.as_deref(), Some("abcdef0"));
    for invalid in [
        "",
        "main",
        "123456",
        "abcdef0\n",
        "012345x",
        &"a".repeat(65),
    ] {
        assert!(source_revision(directory.path(), Some(invalid)).is_err());
    }
    assert!(source_revision(directory.path(), None).is_err());
    Ok(())
}

fn git(directory: &Path, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "user.name=Artifact Test",
            "-c",
            "user.email=artifact@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn repository(directory: &Path) -> Result<(), Box<dyn Error>> {
    git(directory, &["init", "--initial-branch=artifact-test"])?;
    std::fs::write(
        directory.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )?;
    git(directory, &["add", "Cargo.toml"])?;
    git(directory, &["commit", "-m", "artifact fixture"])?;
    Ok(())
}

fn watches(
    paths: &[std::path::PathBuf],
    directory: &Path,
    name: &str,
) -> Result<bool, Box<dyn Error>> {
    let path = git(
        directory,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )?;
    Ok(paths.iter().any(|candidate| candidate == Path::new(&path)))
}

#[test]
fn git_revision_tracks_branch_updates_packed_refs_and_detached_head() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    repository(directory.path())?;
    let initial = source_revision(directory.path(), None)?;
    assert_eq!(
        initial.value.as_deref(),
        Some(git(directory.path(), &["rev-parse", "HEAD"])?.as_str())
    );
    assert!(initial.rerun_paths.iter().all(|path| path.exists()));
    for name in ["HEAD", "refs/heads/artifact-test", "."] {
        assert!(watches(&initial.rerun_paths, directory.path(), name)?);
    }
    assert!(!watches(
        &initial.rerun_paths,
        directory.path(),
        "packed-refs"
    )?);
    git(directory.path(), &["pack-refs", "--all", "--prune"])?;
    let packed = source_revision(directory.path(), None)?;
    assert_eq!(packed.value, initial.value);
    assert!(packed.rerun_paths.iter().all(|path| path.exists()));
    assert!(watches(
        &packed.rerun_paths,
        directory.path(),
        "packed-refs"
    )?);
    assert!(watches(
        &packed.rerun_paths,
        directory.path(),
        "refs/heads"
    )?);
    assert!(!watches(
        &packed.rerun_paths,
        directory.path(),
        "refs/heads/artifact-test"
    )?);
    git(
        directory.path(),
        &["commit", "--allow-empty", "-m", "advance branch"],
    )?;
    let advanced = source_revision(directory.path(), None)?;
    assert_ne!(advanced.value, initial.value);
    assert!(advanced.rerun_paths.iter().all(|path| path.exists()));
    assert!(watches(
        &advanced.rerun_paths,
        directory.path(),
        "refs/heads/artifact-test"
    )?);
    assert!(!watches(
        &advanced.rerun_paths,
        directory.path(),
        "refs/heads"
    )?);
    git(directory.path(), &["checkout", "--detach", "HEAD~1"])?;
    let detached = source_revision(directory.path(), None)?;
    assert_eq!(detached.value, initial.value);
    assert!(detached.rerun_paths.iter().all(|path| path.exists()));
    assert!(watches(&detached.rerun_paths, directory.path(), "HEAD")?);
    Ok(())
}

#[test]
fn linked_worktree_tracks_its_head_pointer_common_dir_and_shared_refs() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let linked = directory.path().join("linked");
    std::fs::create_dir(&primary)?;
    repository(&primary)?;
    git(
        &primary,
        &[
            "worktree",
            "add",
            "-b",
            "artifact-linked",
            linked.to_str().ok_or("non-UTF8 fixture path")?,
        ],
    )?;
    let revision = source_revision(&linked, None)?;
    assert!(revision.rerun_paths.iter().all(|path| path.exists()));
    assert_eq!(
        revision.value.as_deref(),
        Some(git(&linked, &["rev-parse", "HEAD"])?.as_str())
    );
    assert!(
        revision
            .rerun_paths
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == ".git"))
    );
    for name in ["HEAD", "commondir", "refs/heads/artifact-linked"] {
        assert!(watches(&revision.rerun_paths, &linked, name)?);
    }
    let shared_directory = git(
        &linked,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    assert!(
        revision
            .rerun_paths
            .iter()
            .any(|path| path == Path::new(&shared_directory))
    );
    assert!(!watches(&revision.rerun_paths, &linked, "packed-refs")?);
    git(&primary, &["pack-refs", "--all", "--prune"])?;
    let packed = source_revision(&linked, None)?;
    assert_eq!(packed.value, revision.value);
    assert!(packed.rerun_paths.iter().all(|path| path.exists()));
    assert!(watches(&packed.rerun_paths, &linked, "packed-refs")?);
    assert!(watches(&packed.rerun_paths, &linked, "refs/heads")?);
    git(
        &linked,
        &["commit", "--allow-empty", "-m", "advance linked branch"],
    )?;
    assert_ne!(source_revision(&linked, None)?.value, revision.value);
    git(&linked, &["checkout", "-b", "artifact-switched"])?;
    let switched = source_revision(&linked, None)?;
    assert!(switched.rerun_paths.iter().all(|path| path.exists()));
    assert!(watches(&switched.rerun_paths, &linked, "HEAD")?);
    assert!(watches(&switched.rerun_paths, &linked, "commondir")?);
    assert!(watches(
        &switched.rerun_paths,
        &linked,
        "refs/heads/artifact-switched"
    )?);
    assert!(!watches(
        &switched.rerun_paths,
        &linked,
        "refs/heads/artifact-linked"
    )?);
    Ok(())
}

#[test]
fn absent_vcs_metadata_watches_the_nearest_existing_parent_then_the_created_file()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("future/package");
    let missing = source_revision(&package, None)?;
    assert!(missing.value.is_none());
    assert_eq!(missing.rerun_paths, [directory.path().to_owned()]);

    std::fs::create_dir_all(&package)?;
    let archive = source_revision(&package, None)?;
    assert!(archive.value.is_none());
    assert_eq!(
        archive.rerun_paths.as_slice(),
        std::slice::from_ref(&package)
    );

    let vcs_path = package.join(".cargo_vcs_info.json");
    std::fs::write(&vcs_path, r#"{"git":{"sha1":"abcdef0"}}"#)?;
    let packaged = source_revision(&package, None)?;
    assert_eq!(packaged.value.as_deref(), Some("abcdef0"));
    assert_eq!(
        packaged.rerun_paths.as_slice(),
        std::slice::from_ref(&vcs_path)
    );

    std::fs::remove_file(vcs_path)?;
    let removed = source_revision(&package, None)?;
    assert!(removed.value.is_none());
    assert_eq!(removed.rerun_paths, [package]);
    Ok(())
}

#[test]
fn extracted_packages_use_their_vcs_data_and_archives_do_not_inherit_parent_git()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    repository(directory.path())?;
    let package = directory.path().join("target/package/fixture");
    std::fs::create_dir_all(&package)?;
    std::fs::write(package.join("Cargo.toml"), MANIFEST)?;
    assert!(source_revision(&package, None)?.value.is_none());
    let vcs_path = package.join(".cargo_vcs_info.json");
    std::fs::write(
        &vcs_path,
        serde_json::to_vec(&serde_json::json!({
            "git": {"sha1": "abcdef0123456789abcdef0123456789abcdef01", "dirty": true},
            "path_in_vcs": "private/source/path"
        }))?,
    )?;
    let packaged = source_revision(&package, None)?;
    assert_eq!(
        packaged.value.as_deref(),
        Some("abcdef0123456789abcdef0123456789abcdef01")
    );
    assert_eq!(packaged.rerun_paths, [vcs_path]);
    let mut metadata = metadata(&[])?;
    metadata.source_revision = packaged.value;
    let rendered = metadata.render("fixture-handoff");
    assert!(!rendered.contains("private/source/path"));
    assert!(!rendered.contains(package.to_str().ok_or("non-UTF8 fixture path")?));
    metadata.source_revision = None;
    assert!(
        metadata
            .render("fixture-handoff")
            .contains("source_revision: None")
    );
    Ok(())
}
