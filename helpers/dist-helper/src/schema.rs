use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn generate(root: &Path, check: bool) -> Result<()> {
    let path = schema_path(root);
    let rendered = render()?;
    if check {
        let current = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {} - run `cargo run -p dist-helper -- generate-schema`",
                path.display()
            )
        })?;
        if current != rendered {
            bail!(
                "{} is stale - run `cargo run -p dist-helper -- generate-schema` and commit it",
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

fn schema_path(root: &Path) -> PathBuf {
    root.join("dist")
        .join("schema")
        .join("bitrouter.config.schema.json")
}

fn render() -> Result<String> {
    let mut root = serde_json::to_value(schemars::schema_for!(bitrouter_sdk::config::Config))
        .context("serializing generated schema")?;
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
