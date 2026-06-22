//! Workspace automation tasks. Run via `cargo xtask <task>`.
//!
//! Tasks:
//! - `generate-schema` — derive the JSON Schema for `bitrouter.yaml` /
//!   `bitrouter.json` from the canonical [`bitrouter_sdk::config::Config`]
//!   serde structs and write it to `schemas/bitrouter.config.schema.json`.
//! - `generate-schema --check` — regenerate in memory and fail if the committed
//!   schema is stale (the CI drift guard).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

/// Workspace version, stamped into the schema `$id`. Every workspace crate
/// shares `version.workspace`, so xtask's own version is the workspace version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The task is the first non-flag argument, so flag order doesn't matter
    // (`xtask --check generate-schema` and `xtask generate-schema --check` are
    // equivalent).
    let task = args.iter().find(|a| !a.starts_with("--")).cloned();
    let check = args.iter().any(|a| a == "--check");
    let result = match task.as_deref() {
        Some("generate-schema") => generate_schema(check),
        Some(other) => Err(anyhow::anyhow!("unknown task '{other}'")),
        None => Err(anyhow::anyhow!(
            "usage: cargo xtask generate-schema [--check]"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Absolute path to `schemas/bitrouter.config.schema.json`, resolved from the
/// xtask crate dir's parent (the workspace root) so it is independent of the
/// caller's working directory.
fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|root| root.join("schemas").join("bitrouter.config.schema.json"))
        .unwrap_or_else(|| PathBuf::from("schemas/bitrouter.config.schema.json"))
}

/// Render the canonical schema as pretty JSON with a trailing newline.
fn render_schema() -> Result<String> {
    let mut root = serde_json::to_value(schemars::schema_for!(bitrouter_sdk::config::Config))
        .context("serializing generated schema")?;
    // Stamp identity onto the root so a published copy is self-describing and
    // tools (SchemaStore, IDEs) can pin a version.
    if let Some(obj) = root.as_object_mut() {
        obj.insert(
            "$id".to_string(),
            serde_json::Value::String(format!(
                "https://bitrouter.dev/schema/v{VERSION}/config.schema.json"
            )),
        );
        obj.insert(
            "title".to_string(),
            serde_json::Value::String("BitRouter config".to_string()),
        );
    }
    let mut out = serde_json::to_string_pretty(&root).context("formatting schema JSON")?;
    out.push('\n');
    Ok(out)
}

fn generate_schema(check: bool) -> Result<()> {
    let path = schema_path();
    let rendered = render_schema()?;
    if check {
        let current = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {} — run `cargo xtask generate-schema`",
                path.display()
            )
        })?;
        if current != rendered {
            bail!(
                "{} is stale — the config structs changed but the schema was not \
                 regenerated. Run `cargo xtask generate-schema` and commit the result.",
                path.display()
            );
        }
        println!("schema is up to date: {}", path.display());
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}
