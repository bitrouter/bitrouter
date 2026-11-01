// Agent/runtime catalogs retain their independent pre-v1 contract.
use anyhow::{Context, Result, bail};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
#[derive(Debug)]
struct LoadedRegistry {
    agent_files: Vec<LoadedAgentFile>,
    runtimes: Vec<LoadedRuntime>,
}
#[derive(Debug)]
struct LoadedAgentFile {
    path: PathBuf,
    agents: Vec<CanonicalAgent>,
}
#[derive(Debug)]
struct LoadedRuntime {
    path: PathBuf,
    data: RuntimeFile,
}
impl LoadedRegistry {
    fn agents(&self) -> impl Iterator<Item = &CanonicalAgent> {
        self.agent_files.iter().flat_map(|file| file.agents.iter())
    }
}
fn load_registry(root: &Path) -> Result<LoadedRegistry> {
    let registry = root.join("registry");
    let agent_files = optional_yaml_files(&registry.join("agents"))?
        .into_iter()
        .map(|path| {
            Ok(LoadedAgentFile {
                agents: read_yaml(&path)?,
                path,
            })
        })
        .collect::<Result<_>>()?;
    let runtimes = optional_yaml_files(&registry.join("runtimes"))?
        .into_iter()
        .map(|path| {
            Ok(LoadedRuntime {
                data: read_yaml(&path)?,
                path,
            })
        })
        .collect::<Result<_>>()?;
    Ok(LoadedRegistry {
        agent_files,
        runtimes,
    })
}
pub(super) fn build(root: &Path) -> Result<(String, String)> {
    let loaded = load_registry(root)?;
    validate_loaded(&loaded)?;
    // The agent view mirrors the model view: one entry per curated agent,
    // carrying every runtime that can run it. An addressable `<runtime>/<agent>`
    // is never declared — it exists because a runtime lists the agent, exactly
    // as a routable model exists because a provider lists it.
    let mut run_by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for runtime in &loaded.runtimes {
        for entry in &runtime.data.agents {
            let mut value = serde_json::to_value(entry).context("serializing runtime agent")?;
            let obj = value
                .as_object_mut()
                .context("runtime agent must serialize as object")?;
            obj.remove("id");
            obj.insert(
                "runtime".to_string(),
                Value::String(runtime.data.name.clone()),
            );
            run_by.entry(entry.id.clone()).or_default().push(value);
        }
    }
    for runtimes_for_agent in run_by.values_mut() {
        runtimes_for_agent.sort_by(|a, b| a["runtime"].as_str().cmp(&b["runtime"].as_str()));
    }

    let mut catalog: Vec<CanonicalAgent> = loaded.agents().cloned().collect();
    catalog.sort_by(|a, b| a.id.cmp(&b.id));
    let mut agents = Vec::with_capacity(catalog.len());
    for agent in catalog {
        let mut value = serde_json::to_value(&agent).context("serializing canonical agent")?;
        value
            .as_object_mut()
            .context("canonical agent must serialize as object")?
            .insert(
                "runtimes".to_string(),
                Value::Array(run_by.remove(&agent.id).unwrap_or_default()),
            );
        agents.push(value);
    }

    let mut runtimes = Vec::with_capacity(loaded.runtimes.len());
    for runtime in &loaded.runtimes {
        let mut value = serde_json::to_value(&runtime.data).context("serializing runtime")?;
        value
            .as_object_mut()
            .context("runtime must serialize as object")?
            .insert("id".to_string(), Value::String(runtime.data.name.clone()));
        runtimes.push(value);
    }
    runtimes.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));

    Ok((serialize_data(agents)?, serialize_data(runtimes)?))
}
fn serialize_data(data: Vec<Value>) -> Result<String> {
    let value = sort_value(json!({ "data": data }));
    let mut out = serde_json::to_string_pretty(&value).context("formatting dist JSON")?;
    out.push('\n');
    Ok(out)
}

/// Recursively key-sort a JSON value so a generated artifact's bytes do not
/// depend on map iteration order.
///
/// `pub(crate)` because `schema` needs the same guarantee for the same reason —
/// see its `render`.
pub(crate) fn sort_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sort_value).collect()),
        Value::Object(obj) => {
            let mut sorted = Map::new();
            let mut keys: Vec<_> = obj.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if let Some(value) = obj.get(&key) {
                    sorted.insert(key, sort_value(value.clone()));
                }
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

/// YAML files under `dir`, or none when the directory does not exist.
fn optional_yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_yaml_files(dir, &mut files)?;
    Ok(files)
}

fn collect_yaml_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_yaml_files(&path, files)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext == "yaml" || ext == "yml" {
            files.push(path);
        }
    }
    files.sort();
    Ok(())
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_saphyr::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn validate_loaded(registry: &LoadedRegistry) -> Result<Vec<String>> {
    let mut issues = Vec::new();
    let mut advisories = Vec::new();
    let mut canonical_agents = HashSet::new();
    for agent_file in &registry.agent_files {
        let file = path_label(&agent_file.path);
        for agent in &agent_file.agents {
            if !canonical_agents.insert(agent.id.as_str()) {
                issues.push(format!("registry/agents: duplicate agent '{}'", agent.id));
            }
            validate_agent(agent, &file, &mut issues);
        }
    }

    let mut runtime_names = HashMap::new();
    for runtime in &registry.runtimes {
        validate_runtime(
            runtime,
            &canonical_agents,
            &mut runtime_names,
            &mut issues,
            &mut advisories,
        );
    }

    validate_agent_runtime_pairs(registry, &mut issues);

    if !issues.is_empty() {
        bail!("registry validation failed:\n  - {}", issues.join("\n  - "));
    }
    advisories.sort();
    Ok(advisories)
}

fn validate_agent(agent: &CanonicalAgent, file: &str, issues: &mut Vec<String>) {
    if !valid_slug(&agent.id, false) {
        issues.push(format!(
            "{file}: agent id '{}' must be a bare lowercase slug (no vendor prefix — \
             the only prefix an agent id carries is its runtime)",
            agent.id
        ));
    }
    for (field, value) in [
        ("name", &agent.name),
        ("description", &agent.description),
        ("package_marker", &agent.package_marker),
    ] {
        if value.trim().is_empty() {
            issues.push(format!("{file}: agent '{}' has an empty {field}", agent.id));
        }
    }
    validate_https(&agent.project_url, file, "project_url", issues);
    if agent.acp.protocol_version == 0 {
        issues.push(format!(
            "{file}: agent '{}' has acp.protocol_version 0; ACP versions start at 1",
            agent.id
        ));
    }
    match &agent.routing {
        AgentRouting::Env {
            base_url_env,
            auth_env,
            ..
        } => {
            for (field, value) in [("base_url_env", base_url_env), ("auth_env", auth_env)] {
                if value.trim().is_empty() {
                    issues.push(format!(
                        "{file}: agent '{}' routing.{field} must name a variable",
                        agent.id
                    ));
                }
            }
        }
        AgentRouting::ConfigFile {
            dir,
            file: config_file,
            skeleton,
            models,
            default_model,
            mcp,
            env,
            args,
        } => {
            if let Some(dir) = dir {
                validate_relative_path(&agent.id, "routing.dir", dir, file, issues);
            }
            validate_relative_path(&agent.id, "routing.file", config_file, file, issues);
            // The renderer writes `dir.join(file)` and creates only `dir`, so a
            // nested filename fails at launch with a missing-directory error.
            if config_file.contains('/') {
                issues.push(format!(
                    "{file}: agent '{}' routing.file must be a filename — put any \
                     subdirectory in routing.dir, which is the directory the renderer creates",
                    agent.id
                ));
            }
            let parsed: Option<Value> = match serde_json::from_str(skeleton) {
                Ok(value @ Value::Object(_)) => Some(value),
                Ok(_) => {
                    issues.push(format!(
                        "{file}: agent '{}' routing.skeleton must be a JSON object",
                        agent.id
                    ));
                    None
                }
                Err(error) => {
                    issues.push(format!(
                        "{file}: agent '{}' routing.skeleton is not valid JSON: {error}",
                        agent.id
                    ));
                    None
                }
            };
            // Placeholders are checked in the parsed *string leaves*, not the
            // raw text, because that is precisely what the renderer
            // substitutes into — a JSON skeleton is full of braces that are
            // structure, not placeholders.
            if let Some(parsed) = &parsed {
                validate_string_leaves(&agent.id, "routing.skeleton", parsed, file, issues);
            }
            // The renderer replaces values in place and only ever *appends*
            // new keys, so a model list whose key is missing from the skeleton
            // would land at the end of its parent rather than where the
            // harness expects it. Catch that here, not in a diff of rendered
            // bytes.
            if let Some(list) = models
                && validate_pointer(&agent.id, "routing.models.at", &list.at, file, issues)
                && let Some(parsed) = &parsed
                && parsed.pointer(&list.at).is_none()
            {
                issues.push(format!(
                    "{file}: agent '{}' routing.models.at '{}' is not a key in the \
                     skeleton — the model list would be appended instead of landing \
                     where the harness expects it",
                    agent.id, list.at
                ));
            }
            if let Some(default) = default_model
                && validate_pointer(
                    &agent.id,
                    "routing.default_model.at",
                    &default.at,
                    file,
                    issues,
                )
                && let Some(parsed) = &parsed
            {
                validate_pointer_parents(
                    &agent.id,
                    "routing.default_model.at",
                    &default.at,
                    parsed,
                    file,
                    issues,
                );
            }
            if let Some(mcp) = mcp
                && validate_pointer(&agent.id, "routing.mcp.at", &mcp.at, file, issues)
                && let Some(parsed) = &parsed
            {
                validate_pointer_parents(
                    &agent.id,
                    "routing.mcp.at",
                    &mcp.at,
                    parsed,
                    file,
                    issues,
                );
            }
            if env.is_empty() {
                issues.push(format!(
                    "{file}: agent '{}' has `routing.kind: config_file` but sets no \
                     variables, so nothing would point the harness at the synthesized \
                     config",
                    agent.id
                ));
            }
            for entry in env {
                validate_placeholders(
                    &agent.id,
                    &format!("routing.env.{}", entry.name),
                    &entry.value,
                    ENV_PLACEHOLDERS,
                    file,
                    issues,
                );
            }
            for arg in args.always.iter().chain(&args.with_default_model) {
                validate_placeholders(
                    &agent.id,
                    "routing.args",
                    arg,
                    ARG_PLACEHOLDERS,
                    file,
                    issues,
                );
            }
        }
        AgentRouting::CodexArgs => {}
    }
}

/// Checks that need both halves of the primitive pair in hand.
///
/// These are the rules a per-file pass cannot see, and each of them guards a
/// failure that would otherwise land far from its cause — a build error, or a
/// harness that launches unrouted.
fn validate_agent_runtime_pairs(registry: &LoadedRegistry, issues: &mut Vec<String>) {
    let agents: Vec<&CanonicalAgent> = registry.agents().collect();

    // An agent no runtime lists produces `runtimes: []` in the dist artifact,
    // and `apps/bitrouter/build.rs` then fails the *compile* — after both
    // `registry validate` and `dist-helper check` passed.
    let listed: HashSet<&str> = registry
        .runtimes
        .iter()
        .flat_map(|runtime| runtime.data.agents.iter())
        .map(|entry| entry.id.as_str())
        .collect();
    for agent in &agents {
        if !listed.contains(agent.id.as_str()) {
            issues.push(format!(
                "registry/agents: '{}' is listed by no runtime, so nothing can run it",
                agent.id
            ));
        }
    }

    // `package_marker` is how a user-renamed `agents:` entry is mapped back to
    // its routing. A marker that does not occur in the agent's own invocation
    // matches nothing, and the harness launches unrouted with no error.
    for runtime in &registry.runtimes {
        let file = path_label(&runtime.path);
        for entry in &runtime.data.agents {
            let Some(agent) = agents.iter().find(|agent| agent.id == entry.id) else {
                continue;
            };
            let AgentTransport::Stdio { command, args } = &entry.transport;
            let present = command.contains(&agent.package_marker)
                || args.iter().any(|arg| arg.contains(&agent.package_marker));
            if !present {
                issues.push(format!(
                    "{file}: agent '{}' has package_marker '{}', which appears nowhere in its \
                     invocation — invocation matching would never map it back to its routing",
                    entry.id, agent.package_marker
                ));
            }
        }
    }

    // One marker containing another would mis-route the first harness as the
    // second, since matching is a substring test over the invocation.
    for outer in &agents {
        for inner in &agents {
            if outer.id != inner.id && outer.package_marker.contains(&inner.package_marker) {
                issues.push(format!(
                    "registry/agents: '{}' package_marker '{}' contains '{}' from '{}', so an \
                     invocation would match both",
                    outer.id, outer.package_marker, inner.package_marker, inner.id
                ));
            }
        }
    }
}

/// Placeholders each part of a routing block may use. Context-specific,
/// because they resolve at different moments: the skeleton is rendered before
/// the file has a path, and `{default_model}` exists only where a default was
/// resolved.
const SKELETON_PLACEHOLDERS: &[&str] = &["base_url_v1", "auth"];
const ENV_PLACEHOLDERS: &[&str] = &["dir", "file", "auth"];
const ARG_PLACEHOLDERS: &[&str] = &["default_model"];

/// Every `{…}` span in `value` must name a placeholder the renderer knows.
fn validate_placeholders(
    agent_id: &str,
    field: &str,
    value: &str,
    allowed: &[&str],
    file: &str,
    issues: &mut Vec<String>,
) {
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            issues.push(format!(
                "{file}: agent '{agent_id}' {field} has an unterminated placeholder"
            ));
            return;
        };
        let placeholder = &rest[open + 1..open + close];
        if !allowed.contains(&placeholder) {
            issues.push(format!(
                "{file}: agent '{agent_id}' {field} uses unknown placeholder \
                 '{{{placeholder}}}' (known here: {})",
                allowed.join(", ")
            ));
        }
        rest = &rest[open + close + 1..];
    }
}

/// Check every string leaf of a parsed skeleton for unknown placeholders.
fn validate_string_leaves(
    agent_id: &str,
    field: &str,
    value: &Value,
    file: &str,
    issues: &mut Vec<String>,
) {
    match value {
        Value::String(text) => {
            validate_placeholders(agent_id, field, text, SKELETON_PLACEHOLDERS, file, issues);
        }
        Value::Array(items) => {
            for item in items {
                validate_string_leaves(agent_id, field, item, file, issues);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                validate_string_leaves(agent_id, field, item, file, issues);
            }
        }
        _ => {}
    }
}

/// Every parent segment of a filled pointer must already be an object in the
/// skeleton, or be absent.
///
/// The renderer creates missing intermediates but cannot descend through a
/// string or an array, so a skeleton of `{"model": "x"}` with
/// `default_model.at: /model/default` validates on shape and then fails at
/// launch.
fn validate_pointer_parents(
    agent_id: &str,
    field: &str,
    pointer: &str,
    skeleton: &Value,
    file: &str,
    issues: &mut Vec<String>,
) {
    let segments: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    let mut cursor = skeleton;
    for segment in segments.iter().take(segments.len().saturating_sub(1)) {
        let Some(map) = cursor.as_object() else {
            issues.push(format!(
                "{file}: agent '{agent_id}' {field} '{pointer}' descends through a non-object \
                 in the skeleton"
            ));
            return;
        };
        match map.get(*segment) {
            // Absent is fine — the renderer creates it.
            None => return,
            Some(next) => cursor = next,
        }
    }
}

/// A synthesized config's directory and filename must stay inside the
/// per-launch scratch directory.
///
/// The launch-time renderer is not asked to re-check this, because it joins
/// these onto the scratch path directly — which is exactly why the value must
/// be rejected here, before it is ever published to a fetched artifact.
fn validate_relative_path(
    agent_id: &str,
    field: &str,
    value: &str,
    file: &str,
    issues: &mut Vec<String>,
) {
    // Every platform's escaping forms are rejected on every platform: the
    // validator and the renderer need not run on the same host, so the verdict
    // may not depend on which one it is. `..\\evil` and `C:\\evil` escape on
    // Windows, and `/etc/profile` escapes on Unix.
    //
    // `has_root`, not `is_absolute`: on Windows a path is absolute only with a
    // drive or UNC prefix, so `/etc/profile` is merely rooted there and would
    // pass a validator running on Windows.
    let windows_drive = value.len() >= 2
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':';
    let offending = value.is_empty()
        || Path::new(value).has_root()
        || windows_drive
        || value.contains('\\')
        || value.split('/').any(|segment| segment == "..");
    if offending {
        issues.push(format!(
            "{file}: agent '{agent_id}' {field} must be a relative path inside the \
             per-launch directory"
        ));
    }
}

/// A JSON pointer the renderer can follow. Returns whether it is well formed,
/// so a caller can skip checks that would be meaningless otherwise.
fn validate_pointer(
    agent_id: &str,
    field: &str,
    pointer: &str,
    file: &str,
    issues: &mut Vec<String>,
) -> bool {
    // `~` is rejected so the validator's RFC 6901 reader and the renderer's
    // literal `/` split cannot disagree about what a pointer means.
    let well_formed = pointer.starts_with('/')
        && pointer.len() > 1
        && !pointer.contains('~')
        && !pointer.split('/').skip(1).any(str::is_empty);
    if !well_formed {
        issues.push(format!(
            "{file}: agent '{agent_id}' {field} must be a JSON pointer like '/a/b'"
        ));
    }
    well_formed
}

/// Package runners whose invocation fetches the package, so the version in the
/// spec is what decides which code runs.
const PACKAGE_RUNNERS: &[&str] = &["npx", "uvx"];

fn validate_runtime<'a>(
    runtime: &'a LoadedRuntime,
    canonical_agents: &HashSet<&str>,
    names: &mut HashMap<&'a str, String>,
    issues: &mut Vec<String>,
    advisories: &mut Vec<String>,
) {
    let file = path_label(&runtime.path);
    let data = &runtime.data;
    if !valid_provider_name(&data.name) {
        issues.push(format!(
            "{file}: runtime name '{}' must be lowercase alphanumeric with '-' or '_'",
            data.name
        ));
    }
    if runtime.path.file_stem().and_then(|s| s.to_str()) != Some(data.name.as_str()) {
        issues.push(format!(
            "{file}: runtime name '{}' must equal the filename stem",
            data.name
        ));
    }
    if let Some(previous) = names.insert(data.name.as_str(), file.clone()) {
        issues.push(format!(
            "{file}: duplicate runtime name '{}' (also in {previous})",
            data.name
        ));
    }
    if data.agents.is_empty() {
        issues.push(format!(
            "{file}: runtime '{}' lists no agents, so nothing can launch through it",
            data.name
        ));
    }

    let mut seen = HashSet::new();
    for entry in &data.agents {
        if !seen.insert(entry.id.as_str()) {
            issues.push(format!(
                "{file}: runtime '{}' lists agent '{}' twice",
                data.name, entry.id
            ));
        }
        if !canonical_agents.contains(entry.id.as_str()) {
            advisories.push(format!(
                "{file}: agent '{}' not in curated registry/agents",
                entry.id
            ));
        }
        let AgentTransport::Stdio { command, args } = &entry.transport;
        if command.trim().is_empty() {
            issues.push(format!(
                "{file}: agent '{}' has an empty stdio command",
                entry.id
            ));
            continue;
        }
        let record = entry.conformance.as_ref().map(|c| &c.acp_compat_1);
        if PACKAGE_RUNNERS.contains(&command.as_str()) {
            match package_spec(args) {
                Some(spec) if package_spec_is_pinned(spec) => {}
                // A conformance record names the `agent_version` it exercised.
                // A floating tag cannot honestly supply one — whatever the
                // suite ran against is not what the next install will fetch —
                // so recording a result promotes this from advisory to error.
                Some(spec) if record.is_some() => issues.push(format!(
                    "{file}: agent '{}' carries a conformance record but its invocation is \
                     unpinned ('{spec}'), so the record cannot describe what a user would run",
                    entry.id
                )),
                Some(spec) => advisories.push(format!(
                    "{file}: agent '{}' is unpinned ('{spec}') — a floating tag lets the \
                     fetched catalog choose which code runs, and a conformance record \
                     cannot name an agent_version",
                    entry.id
                )),
                None => issues.push(format!(
                    "{file}: agent '{}' runs '{command}' with no package argument",
                    entry.id
                )),
            }
        } else if entry.requires_binary.is_none() {
            issues.push(format!(
                "{file}: agent '{}' runs '{command}', which is not a package runner, so \
                 it must declare `requires_binary`",
                entry.id
            ));
        }
        match record {
            Some(record) => validate_conformance(record, &entry.id, data.status, &file, issues),
            None => advisories.push(format!(
                "{file}: agent '{}' has no {SUITE} record — run \
                 `bro agents conformance {}/{}`",
                entry.id, data.name, entry.id
            )),
        }
    }
}

/// The suite whose records this registry understands.
const SUITE: &str = "acp_compat_1";

/// Check a conformance record's provenance, and refuse to serve an agent whose
/// own record says it does not work.
fn validate_conformance(
    record: &ConformanceRecord,
    agent_id: &str,
    runtime_status: EntryStatus,
    file: &str,
    issues: &mut Vec<String>,
) {
    if record.suite_version.trim().is_empty() {
        issues.push(format!(
            "{file}: agent '{agent_id}' conformance record has no suite_version, so nothing \
             says which checks it passed"
        ));
    }
    if !valid_yyyy_mm_dd(&record.as_of) {
        issues.push(format!(
            "{file}: agent '{agent_id}' conformance as_of '{}' is not YYYY-MM-DD",
            record.as_of
        ));
    }
    if record.measured_by.trim().is_empty() {
        issues.push(format!(
            "{file}: agent '{agent_id}' conformance record has no measured_by"
        ));
    } else if record.measured_by != "bitrouter" {
        // A cited third-party result must be checkable, or it is just a claim
        // wearing a record's clothes.
        match &record.source_url {
            Some(url) => validate_https(url, file, "conformance.source_url", issues),
            None => issues.push(format!(
                "{file}: agent '{agent_id}' conformance is measured_by '{}' but cites no \
                 source_url",
                record.measured_by
            )),
        }
    }
    // The gate: an active runtime must not serve an agent whose own record
    // reports a failed tier. A tier that simply was not run is absent, and
    // that stays permitted — the advisory above is how it surfaces.
    if runtime_status == EntryStatus::Active {
        for (tier, outcome) in [
            ("handshake", record.handshake),
            ("routability", record.routability),
            ("lifecycle", record.lifecycle),
        ] {
            if outcome == Some(TierOutcome::Fail) {
                issues.push(format!(
                    "{file}: agent '{agent_id}' records {SUITE} {tier}: fail, so it cannot be \
                     served by an active runtime"
                ));
            }
        }
    }
}

/// The package spec in a runner invocation: the first argument that is neither
/// a flag nor the `--` separator.
fn package_spec(args: &[String]) -> Option<&str> {
    args.iter()
        .map(String::as_str)
        .find(|arg| *arg != "--" && !arg.starts_with('-'))
}

/// Whether a package spec names an exact version. Scoped npm names lead with
/// `@`, so the version is the *last* `@`-separated segment.
///
/// The test is that the version is exact semver, not that it avoids a list of
/// known-floating tags. A denylist lets `^1.0.0`, `~1.2`, `1.x`, `*`, `>=1`
/// and any unlisted dist-tag (`stable`, `rc`, `nightly`) through, and each of
/// those hands the choice of which code runs back to the registry document —
/// which is the exact thing this rule exists to prevent.
fn package_spec_is_pinned(spec: &str) -> bool {
    let body = spec.strip_prefix('@').unwrap_or(spec);
    let Some((name, version)) = body.rsplit_once('@') else {
        return false;
    };
    !name.is_empty() && is_exact_semver(version)
}

/// `MAJOR.MINOR.PATCH`, optionally with a pre-release or build suffix.
fn is_exact_semver(version: &str) -> bool {
    let (core, suffix) = match version.find(['-', '+']) {
        Some(at) => (&version[..at], Some(&version[at + 1..])),
        None => (version, None),
    };
    let mut parts = core.split('.');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    let core_ok = numeric(parts.next())
        && numeric(parts.next())
        && numeric(parts.next())
        && parts.next().is_none();
    let suffix_ok = suffix.is_none_or(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    core_ok && suffix_ok
}

fn validate_https(url: &str, file: &str, field: &str, issues: &mut Vec<String>) {
    // `api_base` may carry `${VAR}` / `${VAR:-default}` placeholders that the
    // registry merge resolves from the environment (regional / per-account
    // bases like Bedrock's `${AWS_REGION}` or Azure's `${AZURE_OPENAI_RESOURCE}`).
    // Substitute a dummy DNS label first so the template's URL structure is
    // still validated — otherwise the `:` in `:-` reads as a port and parsing
    // fails.
    let resolved = fill_url_placeholders(url);
    if !resolved.starts_with("https://") || reqwest::Url::parse(&resolved).is_err() {
        issues.push(format!("{file}: {field} must be an HTTPS URL"));
    }
}

/// Replace every `${...}` span with a fixed dummy label, so an `api_base`
/// template validates as a concrete URL would.
fn fill_url_placeholders(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find('}') {
            Some(end) => {
                out.push_str("placeholder");
                rest = &rest[start + 2 + end + 1..];
            }
            None => {
                // Unterminated `${` — copy verbatim and stop.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn path_label(path: &Path) -> String {
    path.strip_prefix(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .unwrap_or(path)
        .display()
        .to_string()
}

fn valid_provider_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_slug(value: &str, allow_dot_underscore: bool) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let Some(last) = value.chars().last() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    value.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '-'
            || (allow_dot_underscore && (c == '.' || c == '_'))
    })
}

fn valid_yyyy_mm_dd(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .chars()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// One curated ACP agent — the harness catalog's runtime-independent half.
///
/// Everything here is true wherever the agent runs. Anything that varies by
/// machine (the invocation, whether conformance passed) lives on the runtime
/// entry that lists it, exactly as a model's pricing lives on the provider.
///
/// Ids are **bare** (`claude-acp`, not `anthropic/claude-acp`): the only
/// prefix an agent id ever carries is the runtime it is addressed through
/// (`local/claude-acp`), so a vendor prefix here would make the two
/// indistinguishable. The filename is filing only — unlike `registry/models`,
/// no id/stem relationship is enforced.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CanonicalAgent {
    id: String,
    name: String,
    description: String,
    project_url: String,
    /// Substring that maps a user-renamed `agents:` entry in `bitrouter.yaml`
    /// back to this catalog entry, so routing follows the invocation rather
    /// than the YAML key.
    package_marker: String,
    /// The harness's own native-TUI binary, when it has one. Presence declares
    /// a `bro launch` facet; absence means the agent is ACP-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interactive_binary: Option<String>,
    acp: AgentAcp,
    routing: AgentRouting,
}

/// The ACP contract an agent speaks.
///
/// `capabilities` is deliberately absent until the conformance suite can
/// assert a claim against the `initialize` response — an unverified capability
/// list is worse than none, per the catalog's "omit what you can't verify".
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentAcp {
    /// ACP major version (`1` for the v1 wire semantics this workspace pins).
    protocol_version: u32,
}

/// How an agent's LLM traffic is redirected at the BitRouter gateway.
///
/// The `{base_url}`, `{auth}`, `{model}` and `{dir}` placeholders in any value
/// here are resolved at launch: `{base_url}` by the runtime that runs the
/// agent, the rest per-launch.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AgentRouting {
    /// Set variables on the child process.
    Env {
        /// Var the harness reads its gateway base URL from.
        base_url_env: String,
        /// Var the harness turns into the gateway credential.
        auth_env: String,
        /// Whether `auth_env` is sent as `Authorization: Bearer` (BitRouter's
        /// inbound scheme). `false` means a provider-native header the daemon
        /// accepts only under `skip_auth: true`, and callers warn.
        bearer_auth: bool,
        /// Var that pins the model, when the harness supports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_env: Option<String>,
        /// Fixed vars the redirect needs.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extra: BTreeMap<String, String>,
    },
    /// Codex's one-shot `-c` provider overrides. Named for the harness rather
    /// than the mechanism because that is what it is: the override list is
    /// compiled, so a second agent selecting this would silently receive
    /// Codex's `model_providers.bitrouter.*` arguments. A generic `args` kind
    /// needs its own fields before it can honestly exist.
    CodexArgs,
    /// Render a config file into a per-launch scratch directory and point the
    /// harness at it. The file's fixed structure is `skeleton`; everything
    /// that varies structurally between harnesses is a knob below with a
    /// closed set of values. See `docs/development/catalogs.md` for why this
    /// is not a template language.
    ConfigFile {
        /// Subdirectory under the launch state dir, when the harness wants a
        /// directory of its own. Omitted writes into the state dir itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
        /// Filename within that directory.
        file: String,
        /// JSON structure; string leaves may use `{base_url_v1}` and `{auth}`.
        skeleton: String,
        /// Where the daemon's model catalog lands.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        models: Option<AgentModelList>,
        /// Where the default model lands.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_model: Option<AgentDefaultModel>,
        /// Where injected MCP servers land. Omitted when the harness has no
        /// MCP mechanism to inject into.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp: Option<AgentMcp>,
        /// Variables pointing the harness at the file. Ordered, because the
        /// overlay applies them in this order. Values may use `{dir}`,
        /// `{file}` and `{auth}`.
        env: Vec<AgentEnvVar>,
        #[serde(default, skip_serializing_if = "AgentArgs::is_empty")]
        args: AgentArgs,
    },
}

/// Where and how the model catalog is rendered into a synthesized config.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentModelList {
    /// JSON pointer to the key holding the collection. The key must already
    /// exist in the skeleton, so its position — and the rendered bytes — stay
    /// stable.
    at: String,
    shape: ModelShape,
    order: ModelOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ModelShape {
    /// `{"<id>": {}}`.
    MapOfEmpty,
    /// `[{"id": "<id>"}]`.
    ArrayOfId,
    /// Fully-specified model records, for harnesses whose config validation
    /// rejects anything less.
    ArrayOfProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ModelOrder {
    /// Catalog order, with the pinned model appended only if absent.
    CatalogThenModel,
    /// The pinned model first, then the whole catalog unfiltered.
    ModelThenCatalog,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentDefaultModel {
    /// JSON pointer; intermediate objects are created.
    at: String,
    format: DefaultFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DefaultFormat {
    /// The bare model id.
    Bare,
    /// `bitrouter/<id>`.
    ProviderPrefixed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentMcp {
    at: String,
    entry: McpEntryShape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum McpEntryShape {
    /// An explicit `type` discriminant plus `enabled`, with the stdio command
    /// and its arguments folded into one invocation array.
    OpencodeTyped,
    /// `{command, args}` for stdio, `{url, headers}` for HTTP.
    CommandArgsOrUrl,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentEnvVar {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    /// Always appended to the invocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    always: Vec<String>,
    /// Appended only when a default model exists; `{default_model}` is
    /// substituted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    with_default_model: Vec<String>,
}

impl AgentArgs {
    fn is_empty(&self) -> bool {
        self.always.is_empty() && self.with_default_model.is_empty()
    }
}

/// One machine class agents can execute on. v1 ships `local` only.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFile {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    kind: RuntimeKind,
    status: EntryStatus,
    agents: Vec<RuntimeAgent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeKind {
    /// A child process on this machine, over ACP's canonical stdio transport.
    Local,
}

/// One agent as a runtime runs it: the invocation, and what the machine must
/// already have. The analogue of a provider's per-model entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAgent {
    id: String,
    transport: AgentTransport,
    /// A binary the user must have installed; the invocation does not fetch
    /// it. Required whenever the command is not a package runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requires_binary: Option<String>,
    /// What the ACP-compatibility suite observed for this (agent, runtime)
    /// pair. Absent means **not measured** — never "passes".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conformance: Option<AgentConformance>,
}

/// Conformance records, keyed by suite. Only the suite BitRouter runs has a
/// field; the shape is extensible the way `Benchmarks` is.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentConformance {
    acp_compat_1: ConformanceRecord,
}

/// One suite run against one (agent, runtime) pair.
///
/// Provenance is first-class for the same reason a benchmark score's is: a
/// bare `pass` is not reproducible. `suite_version` says which checks ran,
/// `agent_version` says what actually answered, and `measured_by` keeps a
/// third-party claim from being mistaken for one we ran. A tier that did not
/// run is **absent**, not `skipped` — skipped means the suite decided there
/// was nothing to check.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConformanceRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handshake: Option<TierOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    routability: Option<TierOutcome>,
    /// Session lifecycle — specified but not yet implemented, so no record
    /// carries it today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lifecycle: Option<TierOutcome>,
    suite_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_version: Option<String>,
    /// `bitrouter` for our own runs, otherwise the third-party source.
    measured_by: String,
    /// Snapshot date, `YYYY-MM-DD`.
    as_of: String,
    /// Required when `measured_by` is a third party.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TierOutcome {
    Pass,
    Fail,
    /// The suite determined there was nothing to check — an agent that is
    /// never routed has no routability to verify.
    Skipped,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum AgentTransport {
    /// Launch `command` with `args` and exchange JSON-RPC over the child's
    /// stdio. <https://agentclientprotocol.com/protocol/transports>
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum EntryStatus {
    Active,
    Staging,
    Suspended,
    Withdrawn,
}
