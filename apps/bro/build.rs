//! Embed the build target and source revision without runtime Git discovery.
mod build_catalog;

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-env-changed=BITROUTER_BUILD_REVISION");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    build_catalog::generate(&manifest, &PathBuf::from(std::env::var("OUT_DIR")?))?;
    let explicit = match std::env::var("BITROUTER_BUILD_REVISION") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let revision = bitrouter_build::source_revision(&manifest, explicit.as_deref())?;
    for path in revision.rerun_paths {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    println!(
        "cargo::rustc-env=BRO_BUILD_TARGET={}",
        std::env::var("TARGET")?
    );
    println!(
        "cargo::rustc-env=BRO_BUILD_REVISION={}",
        revision.value.as_deref().unwrap_or_default()
    );
    Ok(())
}
