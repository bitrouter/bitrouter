//! Explicit, source-preserving migration of legacy preset configuration.
//!
//! Only ordinary block mappings are edited. The candidate is checked against
//! the parsed legacy definition before it is offered, and again on apply.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bitrouter_sdk::config::{self, Config};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// A migration result contains paths and identities, never configuration text.
#[derive(Debug, Serialize)]
pub struct MigrationReport {
    pub source: PathBuf,
    pub source_digest: String,
    pub candidate: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub routers: Vec<String>,
    pub applied: bool,
    pub restart_required: bool,
}

impl crate::output::CliReport for MigrationReport {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        h.line(if self.routers.is_empty() {
            "No legacy presets to migrate."
        } else if self.applied {
            "Router migration applied; restart the daemon to activate it."
        } else {
            "Router migration candidate written; source is unchanged."
        })?;
        h.line(&format!("Source: {}", self.source.display()))?;
        h.line(&format!("Source digest: {}", self.source_digest))?;
        if let Some(path) = &self.candidate {
            h.line(&format!("Candidate: {}", path.display()))?;
        }
        if let Some(path) = &self.backup {
            h.line(&format!("Backup: {}", path.display()))?;
        }
        Ok(())
    }
}

fn source_digest(raw: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(raw.as_bytes())))
}

/// Build a candidate, or apply exactly the candidate already reviewed.
///
/// Applying requires both its path and the source digest from preview. A
/// changed source or candidate fails before the original file is modified.
pub async fn migrate(
    path: &Path,
    candidate_path: Option<&Path>,
    apply: bool,
    expected_source: Option<&str>,
) -> Result<MigrationReport> {
    if apply && (candidate_path.is_none() || expected_source.is_none()) {
        bail!("--apply requires --candidate and --source-digest from the preview");
    }
    let absolute_source = std::path::absolute(path)?;
    let path = absolute_source.as_path();
    require_regular_source(path)?;
    let _lock = crate::policy_lock::acquire_publication_lock(path)?;
    require_regular_source(path)?;
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = source_digest(&raw);
    if expected_source.is_some_and(|expected| expected != digest) {
        bail!("source changed since preview; generate and review a new migration candidate");
    }
    let (candidate, ids) = candidate_text(&raw)?;
    let parsed = config::parse(&candidate).context("validating migration candidate")?;
    // Resolves the lock relative to the original file, not the candidate path.
    crate::policy_lock::load_for_config(&parsed, Some(path)).await?;
    let mut report = MigrationReport {
        source: path.to_owned(),
        source_digest: digest,
        candidate: None,
        backup: None,
        routers: ids,
        applied: false,
        restart_required: false,
    };
    if report.routers.is_empty() {
        return Ok(report);
    }
    let destination = candidate_path.map(Path::to_owned).unwrap_or_else(|| {
        path.with_file_name(format!("bitrouter.routers-{}.yaml", uuid::Uuid::new_v4()))
    });
    let destination = std::path::absolute(destination)?;
    if destination == path {
        bail!("candidate path must differ from the source configuration");
    }
    if apply {
        let reviewed = std::fs::read_to_string(&destination)
            .with_context(|| format!("reading reviewed candidate {}", destination.display()))?;
        if reviewed != candidate {
            bail!("candidate differs from the source-preserving migration; preview again");
        }
        let backup = path.with_file_name(format!(
            "bitrouter.before-routers-{}.yaml",
            uuid::Uuid::new_v4()
        ));
        crate::policy_lock::write_new_file_atomic(&backup, raw.as_bytes())?;
        require_regular_source(path)?;
        crate::policy_lock::write_text_atomic_unlocked(path, &raw, &candidate).with_context(|| {
            format!("migration publication failed; backup retained at {}; inspect {} before retrying because publication may have completed before directory sync failed", backup.display(), path.display())
        })?;
        report.backup = Some(backup);
        report.applied = true;
        report.restart_required = true;
    } else {
        crate::policy_lock::write_new_file_atomic(&destination, candidate.as_bytes())?;
    }
    report.candidate = Some(destination);
    Ok(report)
}

fn require_regular_source(path: &Path) -> Result<()> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        bail!(
            "migration requires a regular config file; pass the actual file path for symlink-backed configs"
        );
    }
    Ok(())
}

fn key_at(line: &str, indent: usize) -> Option<&str> {
    if line.bytes().take_while(|byte| *byte == b' ').count() != indent {
        return None;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.is_empty() {
        return None;
    }
    trimmed.split_once(':').map(|(key, _)| key)
}

fn require_block_header(line: &str, key: &str) -> Result<()> {
    let expected = format!("{key}:");
    let tail = line.trim_start().strip_prefix(&expected).ok_or_else(|| {
        anyhow::anyhow!("migration requires an unquoted block mapping for '{key}'")
    })?;
    if !tail.trim().is_empty() && !tail.trim_start().starts_with('#') {
        bail!("migration requires a block mapping for '{key}'; expand flow/anchor syntax first");
    }
    Ok(())
}

fn section(lines: &[&str], key: &str) -> Result<Option<(usize, usize)>> {
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (key_at(line, 0) == Some(key)).then_some(index))
        .collect();
    if starts.len() > 1 {
        bail!("duplicate '{key}' section cannot be migrated");
    }
    let Some(&start) = starts.first() else {
        return Ok(None);
    };
    require_block_header(lines[start], key)?;
    let end = (start + 1..lines.len())
        .find(|&index| key_at(lines[index], 0).is_some())
        .unwrap_or(lines.len());
    Ok(Some((start, end)))
}

fn shifted(line: &str) -> String {
    if line.is_empty() {
        String::new()
    } else {
        format!("  {line}")
    }
}

/// Convert supported block YAML while retaining comments and literal values.
pub fn candidate_text(raw: &str) -> Result<(String, Vec<String>)> {
    let original = config::parse(raw).context("parsing source config")?;
    if original.presets.is_empty() {
        return Ok((raw.to_owned(), Vec::new()));
    }
    let newline = if raw.contains("\r\n") {
        if raw.replace("\r\n", "").contains('\n') {
            bail!("mixed line endings require normalization before migration");
        }
        "\r\n"
    } else {
        "\n"
    };
    if raw.contains('\t') || raw.lines().any(|line| matches!(line.trim(), "---" | "...")) {
        bail!(
            "migration supports one block YAML document without tabs; normalize the layout first"
        );
    }
    let lines: Vec<_> = raw.lines().collect();
    let (start, end) = section(&lines, "presets")?.ok_or_else(|| {
        anyhow::anyhow!("migration requires an unquoted top-level 'presets:' block")
    })?;
    let existing_routers = section(&lines, "routers")?;
    if lines[start..end].iter().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#')
            && (line.contains(": &") || line.contains(": *") || line.starts_with("<<:"))
    }) {
        bail!("block anchors, aliases, and merge keys require an explicit manual migration");
    }
    let mut entries = Vec::new();
    for (index, line) in lines.iter().enumerate().take(end).skip(start + 1) {
        if let Some(id) = key_at(line, 2) {
            require_block_header(line, id)?;
            if !original.presets.contains_key(id) {
                bail!("unsupported preset key '{id}'; use unquoted literal keys");
            }
            entries.push((index, id));
        }
    }
    let ids: Vec<_> = entries.iter().map(|(_, id)| (*id).to_string()).collect();
    if ids.iter().collect::<BTreeSet<_>>().len() != original.presets.len()
        || ids.len() != original.presets.len()
    {
        bail!("unsupported or duplicate preset mapping; use two-space entry indentation");
    }
    let mut converted = Vec::new();
    for (position, &(entry_start, id)) in entries.iter().enumerate() {
        let entry_end = entries.get(position + 1).map_or(end, |entry| entry.0);
        let preset = &original.presets[id];
        if original.routers.contains_key(id) {
            bail!("router '{id}' already exists; resolve the name conflict before migration");
        }
        let mut fields = Vec::new();
        for (index, line) in lines
            .iter()
            .enumerate()
            .take(entry_end)
            .skip(entry_start + 1)
        {
            if let Some(key) = key_at(line, 4) {
                if !matches!(
                    key,
                    "model" | "policy" | "routing" | "system_prompt" | "params"
                ) {
                    bail!("unsupported preset field '{key}' in '{id}'; migrate it manually");
                }
                fields.push((index, key));
            }
        }
        if fields
            .iter()
            .map(|(_, key)| key)
            .collect::<BTreeSet<_>>()
            .len()
            != fields.len()
        {
            bail!("duplicate fields in preset '{id}'");
        }
        if preset
            .model
            .as_ref()
            .is_none_or(|model| model.trim().is_empty())
        {
            bail!("preset '{id}' has no base model; correct it before migration");
        }
        converted.push(lines[entry_start].to_string());
        let first_field = fields.first().map_or(entry_end, |field| field.0);
        converted.extend(
            lines[entry_start + 1..first_field]
                .iter()
                .map(|line| line.to_string()),
        );
        converted.push("    selection:".to_string());
        converted.push(format!(
            "      kind: {}",
            if preset.policy.is_some() {
                "policy"
            } else {
                "model"
            }
        ));
        for defaults in [false, true] {
            if defaults
                && fields
                    .iter()
                    .any(|(_, key)| matches!(*key, "system_prompt" | "params"))
            {
                converted.push("    defaults:".to_string());
            }
            for (field_index, &(field_start, key)) in fields.iter().enumerate() {
                if matches!(key, "system_prompt" | "params") != defaults {
                    continue;
                }
                let field_end = fields
                    .get(field_index + 1)
                    .map_or(entry_end, |field| field.0);
                for (offset, line) in lines[field_start..field_end].iter().enumerate() {
                    if offset == 0 && key == "policy" && preset.policy.is_none() {
                        // Explicit YAML null had no runtime policy binding.
                        // Retain its source/comment without adding an unknown
                        // field to the strict model-selection variant.
                        converted.push(format!("      # {}", line.trim_start()));
                    } else if offset == 0 && key == "model" && preset.policy.is_some() {
                        converted.push(shifted(&line.replacen("model:", "base_model:", 1)));
                    } else {
                        converted.push(shifted(line));
                    }
                }
            }
        }
    }
    let mut output: Vec<String> = lines[..start].iter().map(|line| line.to_string()).collect();
    // Keep the original section comment and any introductory comments.
    let header = lines[start].replacen("presets:", "routers:", 1);
    let intro_end = entries.first().map_or(end, |entry| entry.0);
    let intro: Vec<_> = lines[start + 1..intro_end]
        .iter()
        .map(|line| line.to_string())
        .collect();
    if let Some((router_start, router_end)) = existing_routers {
        // Remove the old preset block first, then append at the original router
        // block boundary with indices adjusted for that removal.
        output.extend(lines[end..].iter().map(|line| line.to_string()));
        let insertion = if router_start < start {
            router_end
        } else {
            router_end - (end - start)
        };
        let mut additions = intro;
        additions.extend(converted);
        if let Some((_, comment)) = header.split_once('#') {
            additions.insert(0, format!("  #{}", comment));
        }
        output.splice(insertion..insertion, additions);
    } else {
        output.push(header);
        output.extend(intro);
        output.extend(converted);
        output.extend(lines[end..].iter().map(|line| line.to_string()));
    }
    let mut candidate = output.join(newline);
    if raw.ends_with('\n') {
        candidate.push_str(newline);
    }
    let migrated = config::parse(&candidate).context(
        "candidate is invalid; invalid legacy IDs require an explicit manual rename and caller update",
    )?;
    verify_equivalence(&original, &migrated)?;
    let mut original_document: serde_json::Value = serde_saphyr::from_str(raw)?;
    let mut migrated_document: serde_json::Value = serde_saphyr::from_str(&candidate)?;
    for document in [&mut original_document, &mut migrated_document] {
        let mapping = document
            .as_object_mut()
            .context("configuration must be a mapping")?;
        mapping.remove("presets");
        mapping.remove("routers");
    }
    if original_document != migrated_document {
        bail!("migration changed configuration outside presets; migrate this YAML layout manually");
    }
    Ok((candidate, ids))
}

fn verify_equivalence(original: &Config, migrated: &Config) -> Result<()> {
    use bitrouter_sdk::config::router::RouterSelection;
    if !migrated.presets.is_empty()
        || migrated.routers.len() != original.routers.len() + original.presets.len()
    {
        bail!("migration did not preserve all definitions");
    }
    for (id, preset) in &original.presets {
        let router = migrated
            .routers
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("missing migrated router '{id}'"))?;
        let (model, policy, routing) = match &router.selection {
            RouterSelection::Model { model, routing } => (model, None, routing),
            RouterSelection::Policy {
                policy,
                base_model,
                routing,
            } => (base_model, Some(policy.as_str()), routing),
        };
        if Some(model.as_str()) != preset.model.as_deref()
            || policy != preset.policy.as_deref()
            || router.defaults.system_prompt != preset.system_prompt
            || router.defaults.params != preset.params
            || serde_json::to_value(routing)? != serde_json::to_value(&preset.routing)?
        {
            bail!("migration changed the effective definition of '{id}'");
        }
    }
    // Existing new routers must retain their complete definition too.
    let existing: BTreeMap<_, _> = original.routers.iter().collect();
    if existing
        .iter()
        .any(|(id, router)| migrated.routers.get(*id) != Some(*router))
    {
        bail!("migration changed an existing router");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_comments_multiline_defaults_and_existing_routers() -> Result<()> {
        let raw = "# config\nrouters:\n  existing:\n    selection:\n      kind: model\n      model: vendor:old\npresets: # legacy\n  coding: # my router\n    model: vendor:strong # base\n    policy: coding\n    system_prompt: |\n      A literal prompt\n      with two lines\n    params:\n      temperature: 0.2 # default\n    routing:\n      only: [vendor]\nchat:\n  model: '@coding' # keep\n";
        let (candidate, ids) = candidate_text(raw)?;
        assert_eq!(ids, ["coding"]);
        assert!(candidate.contains("base_model: vendor:strong # base"));
        assert!(candidate.contains("temperature: 0.2 # default"));
        assert!(candidate.ends_with("chat:\n  model: '@coding' # keep\n"));
        assert!(candidate.contains("# legacy"));
        let (second, second_ids) = candidate_text(&candidate)?;
        assert_eq!(second, candidate);
        assert!(second_ids.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_layouts_that_cannot_be_preserved_safely() {
        for raw in [
            "presets: {coding: {model: 'vendor:base'}}\n",
            "presets:\n  'coding':\n    model: vendor:base\n",
            "presets:\n  coding: &shared\n    model: vendor:base\n",
            "presets:\n  Coding:\n    model: vendor:base\n",
            "presets:\n  coding:\n    model: vendor:base\n    mystery: true\n",
        ] {
            assert!(candidate_text(raw).is_err(), "accepted unsupported layout");
        }
    }

    #[test]
    fn preserves_flow_aliases_only_when_full_config_semantics_match() -> Result<()> {
        let raw = "providers:\n  vendor: &provider\n    api_base: https://example.invalid/v1\npresets:\n  coding:\n    model: vendor:base\n    params: {metadata: [*provider]}\n";
        let (candidate, _) = candidate_text(raw)?;
        assert!(candidate.contains("params: {metadata: [*provider]}"));
        verify_equivalence(&config::parse(raw)?, &config::parse(&candidate)?)?;
        Ok(())
    }

    #[test]
    fn preserves_crlf_outside_the_edited_section() -> Result<()> {
        let raw = "# windows\r\npresets:\r\n  coding:\r\n    model: vendor:base\r\nchat:\r\n  model: '@coding'\r\n";
        let (candidate, _) = candidate_text(raw)?;
        assert!(candidate.starts_with("# windows\r\nrouters:\r\n"));
        assert!(candidate.ends_with("chat:\r\n  model: '@coding'\r\n"));
        assert!(!candidate.replace("\r\n", "").contains('\n'));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_and_preserves_source_permissions() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("bitrouter.yaml");
        let link = directory.path().join("linked.yaml");
        let candidate = directory.path().join("candidate.yaml");
        std::fs::write(&source, "presets:\n  coding:\n    model: vendor:base\n")?;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640))?;
        symlink(&source, &link)?;
        assert!(migrate(&link, None, false, None).await.is_err());
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        let preview = migrate(&source, Some(&candidate), false, None).await?;
        let applied = migrate(
            &source,
            Some(&candidate),
            true,
            Some(&preview.source_digest),
        )
        .await?;
        assert_eq!(
            std::fs::metadata(&source)?.permissions().mode() & 0o777,
            0o640
        );
        let backup = applied.backup.context("missing backup")?;
        assert_eq!(std::fs::metadata(backup)?.permissions().mode() & 0o077, 0);
        assert_eq!(
            std::fs::metadata(candidate)?.permissions().mode() & 0o077,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn preview_apply_backup_and_stale_source_are_safe() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("bitrouter.yaml");
        let candidate = dir.path().join("candidate.yaml");
        let raw = "presets:\n  coding:\n    model: vendor:base\n";
        std::fs::write(&source, raw)?;
        let preview = migrate(&source, Some(&candidate), false, None).await?;
        assert_eq!(std::fs::read_to_string(&source)?, raw);
        std::fs::write(&source, format!("{raw}# concurrent edit\n"))?;
        assert!(
            migrate(
                &source,
                Some(&candidate),
                true,
                Some(&preview.source_digest)
            )
            .await
            .is_err()
        );
        assert!(std::fs::read_to_string(&source)?.contains("# concurrent edit"));
        std::fs::write(&source, raw)?;
        let reviewed = std::fs::read_to_string(&candidate)?;
        std::fs::write(&candidate, format!("{reviewed}# edited candidate\n"))?;
        assert!(
            migrate(
                &source,
                Some(&candidate),
                true,
                Some(&preview.source_digest)
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&source)?, raw);
        std::fs::write(&candidate, reviewed)?;
        let applied = migrate(
            &source,
            Some(&candidate),
            true,
            Some(&preview.source_digest),
        )
        .await?;
        let backup = applied.backup.context("missing backup")?;
        assert_eq!(std::fs::read_to_string(backup)?, raw);
        assert!(applied.applied && applied.restart_required);
        assert!(
            config::parse(&std::fs::read_to_string(&source)?)?
                .presets
                .is_empty()
        );
        assert!(
            migrate(&source, None, false, None)
                .await?
                .routers
                .is_empty()
        );
        Ok(())
    }
}
