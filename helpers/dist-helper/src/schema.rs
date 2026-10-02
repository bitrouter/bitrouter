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
    // Key-sort before rendering, exactly as `registry::serialize_data` does.
    //
    // Not cosmetic. `serde_json::Map` is a `BTreeMap` (sorted) by default and
    // an insertion-ordered map under the `preserve_order` feature — and cargo
    // unifies features across the build graph, so whether this artifact came
    // out sorted depended on whether *some unrelated crate* in `dist-helper`'s
    // tree happened to turn `preserve_order` on. It did: the chain behind
    // `bitrouter-sdk/acp`. Dropping that feature from this helper silently
    // reordered all 3,234 lines of the committed schema without changing a
    // single value.
    //
    // Sorting here makes the bytes a function of the schema alone, so the
    // committed artifact and `dist-helper check` stop being hostage to an
    // unrelated dependency's features.
    let mut out = serde_json::to_string_pretty(&crate::registry::sort_value(root))
        .context("formatting schema JSON")?;
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn rendered_schema_is_key_sorted_everywhere() {
        // Guards the `sort_value` call in `render`.
        //
        // **Be honest about when this can fail.** With `serde_json` built as a
        // `BTreeMap` — the default, and what this workspace resolves to today —
        // every map is sorted already, so deleting the `sort_value` call leaves
        // this test green. Verified, rather than assumed.
        //
        // It is still worth having, because it fires in exactly the situation
        // the sort exists for: the moment any crate in this helper's tree turns
        // `serde_json/preserve_order` on, maps become insertion-ordered and
        // this fails unless `render` sorts. That is not hypothetical — the
        // chain behind `bitrouter-sdk/acp` was doing it, and dropping that
        // feature silently reordered all 3,234 lines of the committed schema
        // without changing a value.
        //
        // The unconditional guard is `dist-helper check` in CI comparing the
        // committed bytes against a fresh render. (`registry.rs`'s
        // `serialize_data_sorts_object_keys_recursively` has the same
        // limitation, for the same reason.)
        //
        // Checked recursively, because the top level alone would still pass
        // with a shallow sort.
        let rendered = render().expect("the config schema renders");
        let value: serde_json::Value =
            serde_json::from_str(&rendered).expect("rendered schema is JSON");

        fn assert_sorted(value: &serde_json::Value, path: &str) {
            match value {
                serde_json::Value::Object(map) => {
                    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
                    let mut expected = keys.clone();
                    expected.sort_unstable();
                    assert_eq!(keys, expected, "keys are unsorted at {path}");
                    for (key, child) in map {
                        assert_sorted(child, &format!("{path}/{key}"));
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        assert_sorted(child, &format!("{path}/{index}"));
                    }
                }
                _ => {}
            }
        }

        assert_sorted(&value, "");
        // Non-vacuity: a schema with nothing in it would satisfy the above.
        assert!(
            value["properties"].as_object().is_some_and(|p| p.len() > 5),
            "the schema rendered nearly empty — this test has stopped testing"
        );
    }
}
