use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn generate(root: &Path, check: bool) -> Result<()> {
    let path = schema_path(root);
    let rendered = render(root)?;
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

fn render(root: &Path) -> Result<String> {
    let cargo = std::env::var_os("CARGO").map_or_else(|| "cargo".into(), |path| path);
    let output = std::process::Command::new(cargo)
        .current_dir(root)
        .args([
            "run",
            "--quiet",
            "--locked",
            "-p",
            "bitrouter",
            "--example",
            "config_schema",
        ])
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running the application configuration schema generator")?;
    if !output.status.success() {
        bail!(
            "application configuration schema generator failed: {}",
            output.status
        );
    }
    String::from_utf8(output.stdout)
        .context("configuration schema generator returned non-UTF-8 output")
}
