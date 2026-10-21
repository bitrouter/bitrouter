//! Conservative block-YAML surgery, checked against the complete semantic document.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::config;
use bitrouter_sdk::mcp::config_routing::{ConfigMcpRoutingTable, McpServerAggregateConfig};
use bitrouter_sdk::mcp::transport::McpServerConfig;
use serde_json::Value;

use super::bad;

pub(super) struct Snapshot {
    pub(super) path: PathBuf,
    pub(super) raw: Option<String>,
}

fn regular(path: &std::path::Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.is_symlink(),
                "MCP editing requires a regular file; pass the actual file path for symlink-backed configurations"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.nlink() == 1,
                    "MCP editing does not replace hard-linked configurations; edit the file manually with $EDITOR"
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("reading configuration metadata"),
    }
}

fn value(raw: &str) -> Result<Value> {
    let value: Value = serde_saphyr::from_str(raw).map_err(|_| {
        bad("invalid YAML/JSON configuration; inspect the source with $EDITOR (values omitted)")
    })?;
    if value.is_null() {
        Ok(serde_json::json!({}))
    } else {
        ensure!(value.is_object(), "configuration must be one mapping");
        Ok(value)
    }
}

impl Snapshot {
    pub(super) fn read(path: PathBuf) -> Result<Self> {
        let raw = if regular(&path)? {
            Some(std::fs::read_to_string(&path).context("reading MCP configuration source")?)
        } else {
            None
        };
        Ok(Self { path, raw })
    }

    pub(super) fn entries(&self) -> Result<BTreeMap<String, McpServerConfig>> {
        let root = value(self.raw.as_deref().unwrap_or("{}"))?;
        match root.get("mcp_servers") {
            None => Ok(BTreeMap::new()),
            Some(entries) => serde_json::from_value(entries.clone()).map_err(|_| {
                bad("mcp_servers must contain single McpServerConfig entries (values omitted)")
            }),
        }
    }

    pub(super) fn entry_value(&self, name: &str) -> Result<Value> {
        value(self.raw.as_deref().unwrap_or("{}"))?
            .get("mcp_servers")
            .and_then(|entries| entries.get(name))
            .cloned()
            .ok_or_else(|| bad("MCP server is not configured"))
    }

    pub(super) fn candidate(&self, name: &str, input: Option<&str>, add: bool) -> Result<String> {
        let raw = self
            .raw
            .as_deref()
            .unwrap_or(crate::commands::STARTER_CONFIG);
        let mut expected = value(raw)?;
        let root = expected
            .as_object_mut()
            .ok_or_else(|| bad("configuration must be a mapping"))?;
        let entries = root
            .entry("mcp_servers")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| bad("mcp_servers must be a mapping"))?;
        ensure!(
            entries.contains_key(name) != add,
            "MCP entry changed or does not match the requested add/edit/remove operation"
        );
        let replacement = if let Some(input) = input {
            let replacement = value(input)?;
            let parsed: McpServerConfig = serde_json::from_value(replacement.clone()).map_err(|_| bad("entry must be a complete single McpServerConfig with name and transport (values omitted)"))?;
            ensure!(
                parsed.name == name,
                "MCP map key and entry name must match; rename requires explicit add/remove"
            );
            entries.insert(name.to_owned(), replacement.clone());
            Some(replacement)
        } else {
            entries.remove(name);
            None
        };
        let updated = replace_entry(raw, name, replacement.as_ref()).with_context(|| {
            format!(
                "cannot safely edit {}; use $EDITOR to expand the mapping first",
                self.path.display()
            )
        })?;
        ensure!(
            value(&updated)? == expected,
            "source-preservation check failed; edit this structure with $EDITOR"
        );
        validate_candidate(&updated)?;
        Ok(updated)
    }

    pub(super) fn commit(self, updated: &str) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.exists()
        {
            crate::paths::ensure_home_directory(parent)?;
        }
        // Shared with router/policy config writers. Held across version check and rename.
        let _lock = crate::policy_lock::acquire_publication_lock(&self.path)?;
        let exists = regular(&self.path)?;
        match self.raw {
            Some(raw) => {
                ensure!(
                    exists,
                    "configuration changed since it was loaded; refusing to overwrite"
                );
                crate::policy_lock::write_text_atomic_unlocked(&self.path, &raw, updated)
                    .context("publishing MCP configuration; if a directory sync failed, inspect saved state before retrying because rename may already have completed")
            }
            None => {
                ensure!(
                    !exists,
                    "configuration appeared since it was loaded; refusing to overwrite"
                );
                crate::policy_lock::write_new_file_atomic(&self.path, updated.as_bytes())
                    .context("creating MCP configuration; inspect saved state before retrying after a directory sync failure")
            }
        }
    }
}

pub(super) fn validate_candidate(raw: &str) -> Result<config::Config> {
    // Resolve a private validation copy only; never serialize substituted values.
    // Like `config validate`, unset variables are placeholders, not endpoint evidence.
    let parsed = config::parse_with(raw, |name| Some(config::env_lookup(name).unwrap_or_else(|| "https://env-placeholder.invalid".into())))
        .map_err(|_| bad("complete candidate configuration is invalid; inspect it with $EDITOR or config validate (values omitted)"))?;
    crate::assemble::validate_host_configuration(&parsed)
        .map_err(|_| bad("candidate violates host configuration rules (values omitted)"))?;
    for (key, entry) in &parsed.mcp_servers {
        ensure!(
            key == &entry.name,
            "MCP map keys and entry names must match"
        );
    }
    ConfigMcpRoutingTable::from_configs(parsed.mcp_servers.clone().into_iter().map(|(key, cfg)| {
        let aggregate = McpServerAggregateConfig { aggregate: cfg.aggregate, tool_prefix: cfg.tool_prefix.clone().unwrap_or_else(|| format!("{key}__")) };
        (key, cfg, aggregate)
    })).map_err(|_| bad("invalid MCP name, transport, URL, aggregate label or duplicate tool prefix (values omitted)"))?;
    Ok(parsed)
}

fn ignored(line: &str) -> bool {
    line.trim().is_empty() || line.trim_start().starts_with('#')
}

fn belongs_to_entry(line: &str, indent: usize) -> bool {
    !ignored(line)
        || (!line.trim().is_empty() && line.bytes().take_while(|b| *b == b' ').count() > indent)
}

fn key(line: &str, indent: usize) -> Result<String> {
    ensure!(
        line.bytes().take_while(|b| *b == b' ').count() == indent,
        "unsupported mapping indentation"
    );
    let line = line.trim_start();
    if line.starts_with('"') {
        let mut parser = serde_json::Deserializer::from_str(line).into_iter::<String>();
        let key = parser
            .next()
            .transpose()
            .map_err(|_| bad("unsupported quoted mapping key"))?
            .ok_or_else(|| bad("missing mapping key"))?;
        ensure!(
            line[parser.byte_offset()..].trim_start().starts_with(':'),
            "unsupported mapping key"
        );
        return Ok(key);
    }
    let (key, _) = line
        .split_once(':')
        .ok_or_else(|| bad("only block mappings can be edited losslessly"))?;
    ensure!(
        !key.is_empty()
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.~".contains(&b)),
        "expand complex mapping keys with $EDITOR first"
    );
    Ok(key.into())
}

fn replace_entry(raw: &str, name: &str, replacement: Option<&Value>) -> Result<String> {
    ensure!(!raw.contains('\t'), "tabs require explicit manual editing");
    let newline = if raw.contains("\r\n") {
        ensure!(
            !raw.replace("\r\n", "").contains('\n'),
            "mixed line endings require explicit manual editing"
        );
        "\r\n"
    } else {
        "\n"
    };
    let lines: Vec<_> = raw.split_inclusive('\n').collect();
    let roots: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !ignored(line) && !line.starts_with(' '))
        .map(|(i, line)| Ok((i, key(line, 0)?)))
        .collect::<Result<_>>()?;
    let section = roots.iter().position(|(_, key)| key == "mcp_servers");
    let (start, end, indent, entries) = if let Some(position) = section {
        let start = roots[position].0;
        // Quoted root headers, anchors, null and flow mappings are deliberately refused.
        let tail = lines[start]
            .strip_prefix("mcp_servers:")
            .ok_or_else(|| bad("expand the mcp_servers header to an unquoted block mapping"))?;
        let content = tail.split('#').next().unwrap_or_default().trim();
        ensure!(
            content.is_empty() || content == "{}",
            "expand flow/anchor mcp_servers with $EDITOR first"
        );
        let end = roots.get(position + 1).map_or(lines.len(), |(i, _)| *i);
        let indent = lines[start + 1..end]
            .iter()
            .filter(|line| !ignored(line))
            .map(|line| line.bytes().take_while(|b| *b == b' ').count())
            .min()
            .unwrap_or(2);
        ensure!(indent > 0, "MCP entries require a block mapping");
        let entries: Vec<_> = (start + 1..end)
            .filter(|&i| {
                !ignored(lines[i]) && lines[i].bytes().take_while(|b| *b == b' ').count() == indent
            })
            .map(|i| Ok((i, key(lines[i], indent)?)))
            .collect::<Result<_>>()?;
        (start, end, indent, entries)
    } else {
        (lines.len(), lines.len(), 2, Vec::new())
    };
    let target = entries.iter().position(|(_, key)| key == name);
    let mut output = raw.to_owned();
    let offset = |i: usize| lines[..i].iter().map(|line| line.len()).sum::<usize>();
    let block = if let Some(replacement) = replacement {
        let serialized =
            serde_saphyr::to_string(replacement).map_err(|_| bad("could not encode MCP entry"))?;
        let prefix = " ".repeat(indent);
        let mut block = format!("{prefix}{}:{newline}", serde_json::to_string(name)?);
        for line in serialized.lines() {
            block.push_str(&format!("{prefix}  {line}{newline}"));
        }
        block
    } else {
        String::new()
    };
    if let Some(position) = target {
        let first = entries[position].0;
        let boundary = entries.get(position + 1).map_or(end, |(i, _)| *i);
        // Keep trailing comments/blank lines, which may document the next sibling.
        let last = (first..boundary)
            .rfind(|&i| belongs_to_entry(lines[i], indent))
            .map_or(first + 1, |i| i + 1);
        output.replace_range(offset(first)..offset(last), &block);
        if replacement.is_none() && entries.len() == 1 {
            let original = lines[start];
            let comment = original
                .find('#')
                .map(|i| original[i..].trim_end())
                .unwrap_or("");
            output.replace_range(
                offset(start)..offset(start + 1),
                &format!(
                    "mcp_servers: {{}}{space}{comment}{newline}",
                    space = if comment.is_empty() { "" } else { " " }
                ),
            );
        }
    } else {
        ensure!(replacement.is_some(), "MCP server is not configured");
        if section.is_none() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push_str(newline);
            }
            output.push_str(&format!("mcp_servers:{newline}{block}"));
        } else {
            let at = (start + 1..end)
                .rfind(|&i| belongs_to_entry(lines[i], indent))
                .map_or(start + 1, |i| i + 1);
            let separation = if at > 0 && !lines[at - 1].ends_with('\n') {
                newline
            } else {
                ""
            };
            output.insert_str(offset(at), &format!("{separation}{block}"));
            // Convert the empty flow map, preserving its original trailing comment.
            if let Some(index) = lines[start]
                .split('#')
                .next()
                .unwrap_or_default()
                .find("{}")
            {
                output.replace_range(offset(start) + index..offset(start) + index + 2, "");
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> String {
        format!("name: {name}\ntransport:\n  type: http\n  url: https://example.com/mcp\n")
    }

    #[test]
    fn preserves_other_nodes_comments_order_and_crlf() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.yaml");
        let original = "# before\r\ninherit_defaults: false\r\nmcp_servers: # MCP heading\r\n  old:\r\n    name: old\r\n    transport: {type: http, url: 'https://example.com/${ENDPOINT}'}\r\n# next heading\r\nchat:\r\n  model: 'unchanged' # keep\r\n";
        std::fs::write(&path, original)?;
        let snap = Snapshot::read(path.clone())?;
        let candidate = snap.candidate("new", Some(&entry("new")), true)?;
        ensure!(candidate.contains("url: 'https://example.com/${ENDPOINT}'"));
        ensure!(candidate.ends_with("# next heading\r\nchat:\r\n  model: 'unchanged' # keep\r\n"));
        ensure!(!candidate.replace("\r\n", "").contains('\n'));
        snap.commit(&candidate)?;
        let snap = Snapshot::read(path.clone())?;
        let replacement = snap.candidate("old", Some(&entry("old")), false)?;
        snap.commit(&replacement)?;
        let snap = Snapshot::read(path.clone())?;
        let removed = snap.candidate("old", None, false)?;
        snap.commit(&removed)?;
        ensure!(Snapshot::read(path)?.entries()?.len() == 1);
        Ok(())
    }

    #[test]
    fn header_braces_and_literal_comment_lines_are_not_reinterpreted() -> Result<()> {
        let snapshot = Snapshot { path: PathBuf::from("/unused/config.yaml"), raw: Some("inherit_defaults: false\nmcp_servers: # keep {} literally\n  old:\n    name: old\n    transport:\n      type: stdio\n      command: |\n        # literal command, not a YAML comment\n# next root comment\nchat: {}\n".into()) };
        let candidate = snapshot.candidate("new", Some(&entry("new")), true)?;
        ensure!(candidate.contains("mcp_servers: # keep {} literally"));
        let next = Snapshot {
            path: snapshot.path.clone(),
            raw: Some(candidate),
        };
        let removed = next.candidate("old", None, false)?;
        ensure!(!removed.contains("literal command, not a YAML comment"));
        ensure!(removed.contains("# next root comment\nchat: {}\n"));
        Ok(())
    }

    #[test]
    fn rejects_stale_or_created_sources_and_preserves_bytes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.yaml");
        let absent = Snapshot::read(path.clone())?;
        let candidate = absent.candidate("new", Some(&entry("new")), true)?;
        std::fs::write(&path, "# concurrent\ninherit_defaults: false\n")?;
        ensure!(absent.commit(&candidate).is_err());
        let snap = Snapshot::read(path.clone())?;
        let candidate = snap.candidate("new", Some(&entry("new")), true)?;
        std::fs::write(&path, "# second concurrent\ninherit_defaults: false\n")?;
        ensure!(snap.commit(&candidate).is_err());
        ensure!(std::fs::read_to_string(path)? == "# second concurrent\ninherit_defaults: false\n");
        Ok(())
    }

    #[test]
    fn concurrent_writers_use_one_lock_and_only_one_snapshot_can_commit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "inherit_defaults: false\n")?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for name in ["one", "two"] {
            let snapshot = Snapshot::read(path.clone())?;
            let candidate = snapshot.candidate(name, Some(&entry(name)), true)?;
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                snapshot.commit(&candidate)
            }));
        }
        let mut passed = 0;
        for worker in workers {
            if worker
                .join()
                .map_err(|_| anyhow::anyhow!("writer thread failed"))?
                .is_ok()
            {
                passed += 1;
            }
        }
        ensure!(passed == 1, "both stale snapshots committed");
        ensure!(Snapshot::read(path)?.entries()?.len() == 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_preserved_and_replaced_symlink_is_never_followed() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "inherit_defaults: false\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
        let snapshot = Snapshot::read(path.clone())?;
        let candidate = snapshot.candidate("one", Some(&entry("one")), true)?;
        snapshot.commit(&candidate)?;
        ensure!(std::fs::metadata(&path)?.permissions().mode() & 0o777 == 0o640);
        let snapshot = Snapshot::read(path.clone())?;
        let candidate = snapshot.candidate("one", None, false)?;
        let moved = dir.path().join("moved.yaml");
        std::fs::rename(&path, &moved)?;
        symlink(&moved, &path)?;
        ensure!(snapshot.commit(&candidate).is_err());
        ensure!(std::fs::symlink_metadata(&path)?.is_symlink());
        ensure!(std::fs::read_to_string(moved)?.contains("one"));
        Ok(())
    }

    #[test]
    fn rejects_complex_layout_invalid_complete_config_and_name_mismatch() -> Result<()> {
        let path = PathBuf::from("/unused/config.yaml");
        for raw in [
            "{inherit_defaults: false, mcp_servers: {}}\n",
            "inherit_defaults: false\nmcp_servers: &shared {}\n",
            "---\ninherit_defaults: false\n",
            "inherit_defaults: false\nrouters: [invalid]\n",
        ] {
            let snap = Snapshot {
                path: path.clone(),
                raw: Some(raw.into()),
            };
            ensure!(
                snap.candidate("new", Some(&entry("new")), true).is_err(),
                "accepted unsupported or invalid source"
            );
        }
        let snap = Snapshot {
            path,
            raw: Some("inherit_defaults: false\n".into()),
        };
        ensure!(snap.candidate("new", Some(&entry("other")), true).is_err());
        Ok(())
    }
}
