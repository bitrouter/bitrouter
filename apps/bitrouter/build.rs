//! Generate gateway identity and bundle the provider registry for offline discovery.

use std::error::Error;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

mod build_artifact;

fn main() -> Result<(), Box<dyn Error>> {
    // Test-only campaigns stay outside ordinary builds, including --all-features.
    // https://doc.rust-lang.org/cargo/reference/build-scripts.html#rustc-check-cfg
    println!("cargo::rustc-check-cfg=cfg(bitrouter_capacity_tests)");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let dest = PathBuf::from(std::env::var("OUT_DIR")?);
    let manifest_path = manifest.join("Cargo.toml");
    println!("cargo::rerun-if-changed={}", manifest_path.display());
    println!("cargo::rerun-if-changed=build_artifact.rs");
    // This is a build-time release override, never a runtime setting. Cargo
    // tracks its presence and value so incremental builds cannot reuse it.
    // https://doc.rust-lang.org/cargo/reference/build-scripts.html#rerun-if-env-changed
    println!("cargo::rerun-if-env-changed=BITROUTER_BUILD_REVISION");
    let explicit_revision = match std::env::var("BITROUTER_BUILD_REVISION") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let revision = bitrouter_build::source_revision(&manifest, explicit_revision.as_deref())?;
    for path in revision.rerun_paths {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    let metadata = build_artifact::BuildMetadata {
        version: std::env::var("CARGO_PKG_VERSION")?,
        source_revision: revision.value,
        target: std::env::var("TARGET")?,
        selection: build_artifact::FeatureSelection::from_manifest(
            &std::fs::read_to_string(manifest_path)?,
            |variable| std::env::var_os(variable).is_some(),
        )?,
    };
    let mut handoff_hash = Sha256::new();
    for file in build_artifact::HANDOFF_SOURCES {
        let path = manifest.join(file);
        println!("cargo::rerun-if-changed={}", path.display());
        handoff_hash.update(std::fs::read(path)?);
    }
    let migration_dir = manifest.join("src/db/migration");
    println!("cargo::rerun-if-changed={}", migration_dir.display());
    let mut migrations = std::fs::read_dir(migration_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    migrations.sort();
    for path in migrations {
        if path.extension().is_some_and(|extension| extension == "rs") {
            println!("cargo::rerun-if-changed={}", path.display());
            handoff_hash.update(std::fs::read(path)?);
        }
    }
    metadata.extend_handoff_hash(&mut handoff_hash)?;
    let handoff_build_id = handoff_hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("cargo::rustc-env=BITROUTER_HANDOFF_BUILD_ID={handoff_build_id}");
    std::fs::write(
        dest.join("artifact_generated.rs"),
        metadata.render(&handoff_build_id),
    )?;
    std::fs::write(
        dest.join("artifact.json"),
        serde_json::to_vec_pretty(&metadata.artifact_json(&handoff_build_id))?,
    )?;
    // The package-local snapshot is generated together with `dist/registry`.
    // Keeping the build input inside the crate is required by `cargo package`,
    // whose verification build cannot read files outside the packaged crate.
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("registry-dist");
    for name in ["registry.json", "source.json"] {
        let path = root.join(name);
        println!("cargo::rerun-if-changed={}", path.display());
        std::fs::copy(
            &path,
            dest.join(if name == "registry.json" {
                "provider_registry.json"
            } else {
                "registry_source.json"
            }),
        )?;
    }
    Ok(())
}
