//! Workflow recipes: `recipes/<slug>/` source → committed
//! `dist/recipes/index.json`.
//!
//! A recipe publishes one reusable routing template together with the measured
//! result of running it against a baseline. Two rules shape everything here:
//!
//! 1. **Anything derivable from the config is derived, never restated.** The
//!    providers, models, and environment variables a recipe needs are extracted
//!    from its named template, so metadata cannot drift from the config it
//!    describes.
//! 2. **Measurements are stored; claims are computed.** `baseline` and `recipe`
//!    carry raw metrics and the deltas the site renders are derived at build
//!    time — a stored percentage can disagree with its own inputs, a computed
//!    one cannot.
//!
//! Only `status: published` recipes reach `dist/`, which is what "evaluated
//! before released" means mechanically: a recipe without an `evaluation` block
//! cannot be published, and a recipe that is not published never reaches the
//! site.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bitrouter_sdk::config::Config;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::registry::{Catalog, serialize_data, valid_slug, valid_yyyy_mm_dd};

/// Where a recipe's source directory is browsable — recorded per entry so the
/// site can link "view source" without hardcoding the repo layout.
const SOURCE_BASE: &str = "https://github.com/bitrouter/bitrouter/tree/main/recipes";
const TEMPLATE_BASE: &str = "https://github.com/bitrouter/bitrouter/tree/main/templates";

pub fn validate(root: &Path) -> Result<()> {
    let loaded = load_recipes(root)?;
    let catalog = crate::registry::catalog(root)?;
    let advisories = validate_loaded(&loaded, &catalog)?;
    let published = loaded
        .iter()
        .filter(|r| r.meta.status == RecipeStatus::Published)
        .count();
    println!(
        "recipes valid: {} published, {} draft",
        published,
        loaded.len() - published
    );
    if !advisories.is_empty() {
        println!("note: {} advisory/advisories:", advisories.len());
        for advisory in &advisories {
            println!("  - {advisory}");
        }
    }
    Ok(())
}

pub fn build(root: &Path, check: bool) -> Result<()> {
    let loaded = load_recipes(root)?;
    let catalog = crate::registry::catalog(root)?;
    validate_loaded(&loaded, &catalog)?;

    let data: Vec<Value> = loaded
        .iter()
        .filter(|recipe| recipe.meta.status == RecipeStatus::Published)
        .map(|recipe| dist_value(recipe, &catalog))
        .collect();
    let count = data.len();
    let rendered = serialize_data(data)?;

    let index_path = dist_dir(root).join("index.json");
    if check {
        let current = fs::read_to_string(&index_path)
            .with_context(|| format!("reading {}", index_path.display()))?;
        if current != rendered {
            bail!(
                "recipes dist is stale - run `cargo run -p dist-helper -- recipes build` and commit dist/recipes"
            );
        }
        println!("recipes dist is up to date: {count} published recipes");
        return Ok(());
    }
    fs::create_dir_all(dist_dir(root))
        .with_context(|| format!("creating {}", dist_dir(root).display()))?;
    fs::write(&index_path, rendered)
        .with_context(|| format!("writing {}", index_path.display()))?;
    println!("wrote dist/recipes/index.json - {count} published recipes");
    Ok(())
}

fn dist_dir(root: &Path) -> PathBuf {
    root.join("dist").join("recipes")
}

// --- source loading ----------------------------------------------------------

struct LoadedRecipe {
    /// Directory name, which is the authoritative slug — `recipe.yaml`'s own
    /// `slug` must agree with it.
    dir_slug: String,
    meta: RecipeFile,
    config_raw: String,
    config: Config,
    policy_lock: Option<LoadedPolicyLock>,
    body_en: String,
    body_zh: Option<String>,
}

struct LoadedPolicyLock {
    raw: String,
    document: RecipePolicyLock,
}

fn load_recipes(root: &Path) -> Result<Vec<LoadedRecipe>> {
    let dir = root.join("recipes");
    let mut dirs = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();

    let mut out = Vec::with_capacity(dirs.len());
    for path in dirs {
        out.push(load_recipe(root, &path)?);
    }
    Ok(out)
}

fn load_recipe(root: &Path, dir: &Path) -> Result<LoadedRecipe> {
    let dir_slug = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();

    let meta_path = dir.join("recipe.yaml");
    let meta_raw = fs::read_to_string(&meta_path)
        .with_context(|| format!("reading {}", meta_path.display()))?;
    let meta: RecipeFile = serde_saphyr::from_str(&meta_raw)
        .with_context(|| format!("parsing {}", meta_path.display()))?;

    let template_dir = root.join("templates").join(&meta.template);
    let config_path = template_dir.join("bitrouter.yaml");
    let config_raw = fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    // Parsed through the loader the daemon itself uses, with **no** environment
    // variables visible: a recipe whose `${VAR}` interpolation only works on the
    // author's machine must fail here, not on the reader's first `bitrouter start`.
    let config = bitrouter_sdk::config::parse_with(&config_raw, |_| None)
        .map_err(|err| anyhow::anyhow!("{err}"))
        .with_context(|| format!("parsing {}", config_path.display()))?;

    let body_path = dir.join("README.md");
    let body_en = fs::read_to_string(&body_path)
        .with_context(|| format!("reading {}", body_path.display()))?;

    Ok(LoadedRecipe {
        dir_slug,
        meta,
        config_raw,
        config,
        policy_lock: load_policy_lock(&template_dir.join("policy-lock.yaml"))?,
        body_en,
        body_zh: read_optional(&dir.join("README.zh.md"))?,
    })
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(raw))
}

fn load_policy_lock(path: &Path) -> Result<Option<LoadedPolicyLock>> {
    let Some(raw) = read_optional(path)? else {
        return Ok(None);
    };
    let document =
        serde_saphyr::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(LoadedPolicyLock { raw, document }))
}

// --- validation --------------------------------------------------------------

fn validate_loaded(recipes: &[LoadedRecipe], catalog: &Catalog) -> Result<Vec<String>> {
    let mut issues = Vec::new();
    let mut advisories = Vec::new();
    let mut seen = BTreeSet::new();

    for recipe in recipes {
        if !seen.insert(recipe.dir_slug.as_str()) {
            issues.push(format!("recipes/{}: duplicate slug", recipe.dir_slug));
        }
        validate_recipe(recipe, catalog, &mut issues, &mut advisories);
    }

    if !issues.is_empty() {
        bail!("recipes validation failed:\n  - {}", issues.join("\n  - "));
    }
    advisories.sort();
    Ok(advisories)
}

fn validate_recipe(
    recipe: &LoadedRecipe,
    catalog: &Catalog,
    issues: &mut Vec<String>,
    advisories: &mut Vec<String>,
) {
    let slug = &recipe.dir_slug;
    let meta = &recipe.meta;
    let published = meta.status == RecipeStatus::Published;

    if !valid_slug(slug, false) {
        issues.push(format!(
            "recipes/{slug}: directory name is not a lowercase kebab-case slug"
        ));
    }
    if meta.slug != *slug {
        issues.push(format!(
            "recipes/{slug}: recipe.yaml slug '{}' does not match the directory name",
            meta.slug
        ));
    }
    if !valid_slug(&meta.template, false) {
        issues.push(format!(
            "recipes/{slug}: template '{}' is not a lowercase kebab-case slug",
            meta.template
        ));
    }
    if !valid_slug(&meta.workflow, false) {
        issues.push(format!(
            "recipes/{slug}: workflow '{}' is not a lowercase kebab-case slug",
            meta.workflow
        ));
    }
    if meta.harness.is_empty() {
        issues.push(format!(
            "recipes/{slug}: harness must list at least one harness"
        ));
    }
    for harness in &meta.harness {
        if !valid_slug(harness, false) {
            issues.push(format!(
                "recipes/{slug}: harness '{harness}' is not a lowercase kebab-case slug"
            ));
        }
    }
    if meta.objectives.is_empty() {
        issues.push(format!(
            "recipes/{slug}: objectives must list at least one of cost / latency / accuracy"
        ));
    }
    if !valid_yyyy_mm_dd(&meta.updated_at) {
        issues.push(format!(
            "recipes/{slug}: updated_at '{}' is not YYYY-MM-DD",
            meta.updated_at
        ));
    }

    // Chinese is carried but not yet demanded: the gallery renders on the
    // site's English-only marketing routes, so a missing translation is an
    // advisory rather than an error. A *present but blank* one is still a bug.
    // Harden this to an error for published recipes once those routes are
    // localized, which is the same day the catalog's `zh` starts being read.
    for (field, text) in [("title", &meta.title), ("description", &meta.description)] {
        if text.en.trim().is_empty() {
            issues.push(format!("recipes/{slug}: {field}.en is empty"));
        }
        match &text.zh {
            Some(zh) if zh.trim().is_empty() => {
                issues.push(format!("recipes/{slug}: {field}.zh is empty"));
            }
            None if published => {
                advisories.push(format!("recipes/{slug}: {field} has no zh translation"))
            }
            _ => {}
        }
    }
    if recipe.body_zh.is_none() {
        advisories.push(format!(
            "recipes/{slug}: no README.zh.md - the Chinese body falls back to English"
        ));
    }

    match &meta.evaluation {
        Some(evaluation) => {
            validate_evaluation(slug, evaluation, issues);
            validate_evaluation_artifacts(recipe, evaluation, issues);
        }
        None if published => issues.push(format!(
            "recipes/{slug}: status is published but no evaluation block is present - a recipe is measured before it is released"
        )),
        None => {}
    }

    validate_config(recipe, catalog, issues);
}

fn validate_evaluation_artifacts(
    recipe: &LoadedRecipe,
    evaluation: &Evaluation,
    issues: &mut Vec<String>,
) {
    let slug = &recipe.dir_slug;
    let config_sha256 = content_sha256(&recipe.config_raw);
    if evaluation.artifacts.config_sha256 != config_sha256 {
        issues.push(format!(
            "recipes/{slug}: evaluation.artifacts.config_sha256 is '{}', but the current template is '{config_sha256}'",
            evaluation.artifacts.config_sha256
        ));
    }

    match &recipe.policy_lock {
        Some(lock) => {
            let policy_lock_sha256 = content_sha256(&lock.raw);
            if evaluation.artifacts.policy_lock_sha256 != policy_lock_sha256 {
                issues.push(format!(
                    "recipes/{slug}: evaluation.artifacts.policy_lock_sha256 is '{}', but the current template is '{policy_lock_sha256}'",
                    evaluation.artifacts.policy_lock_sha256
                ));
            }
        }
        None => issues.push(format!(
            "recipes/{slug}: evaluation binds policy_lock_sha256 but the template has no policy-lock.yaml"
        )),
    }
}

fn content_sha256(raw: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(raw.as_bytes())))
}

fn validate_evaluation(slug: &str, evaluation: &Evaluation, issues: &mut Vec<String>) {
    if evaluation.eval.trim().is_empty() {
        issues.push(format!("recipes/{slug}: evaluation.eval is empty"));
    }
    if evaluation.measured_by.trim().is_empty() {
        issues.push(format!("recipes/{slug}: evaluation.measured_by is empty"));
    }
    // A third-party number must be citable, or it is indistinguishable from one
    // we ran ourselves.
    if evaluation.measured_by != "bitrouter" {
        match &evaluation.source_url {
            None => issues.push(format!(
                "recipes/{slug}: evaluation.source_url is required when measured_by is not 'bitrouter'"
            )),
            Some(url) if !url.starts_with("https://") => issues.push(format!(
                "recipes/{slug}: evaluation.source_url '{url}' is not https"
            )),
            Some(_) => {}
        }
    }
    if !valid_yyyy_mm_dd(&evaluation.as_of) {
        issues.push(format!(
            "recipes/{slug}: evaluation.as_of '{}' is not YYYY-MM-DD",
            evaluation.as_of
        ));
    }
    if evaluation.runs == 0 {
        issues.push(format!(
            "recipes/{slug}: evaluation.runs must be at least 1"
        ));
    }

    for (side, measurement) in [
        ("baseline", &evaluation.baseline),
        ("recipe", &evaluation.recipe),
    ] {
        if let Some(accuracy) = measurement.accuracy
            && !(0.0..=100.0).contains(&accuracy)
        {
            issues.push(format!(
                "recipes/{slug}: evaluation.{side}.accuracy {accuracy} is outside 0..=100"
            ));
        }
        for (field, value) in [
            ("cost_per_task", measurement.cost_per_task),
            ("time_per_task", measurement.time_per_task),
        ] {
            if let Some(value) = value
                && value < 0.0
            {
                issues.push(format!(
                    "recipes/{slug}: evaluation.{side}.{field} {value} is negative"
                ));
            }
        }
    }

    // The whole point of a recipe is the comparison, so the two sides must
    // report the *same* metrics: a delta between a metric measured on one side
    // only is not a delta.
    let baseline_metrics = evaluation.baseline.metrics();
    let recipe_metrics = evaluation.recipe.metrics();
    if recipe_metrics.is_empty() {
        issues.push(format!(
            "recipes/{slug}: evaluation.recipe reports no metrics (accuracy / cost_per_task / time_per_task)"
        ));
    }
    for field in baseline_metrics.symmetric_difference(&recipe_metrics) {
        issues.push(format!(
            "recipes/{slug}: evaluation reports {field} on only one of baseline / recipe"
        ));
    }
    // A percentage delta against a zero baseline is undefined.
    for (field, value) in [
        ("cost_per_task", evaluation.baseline.cost_per_task),
        ("time_per_task", evaluation.baseline.time_per_task),
    ] {
        if value == Some(0.0) {
            issues.push(format!(
                "recipes/{slug}: evaluation.baseline.{field} is 0 - no percentage delta can be computed from it"
            ));
        }
    }
}

/// Cross-check the shipped config against the registry: a recipe may only
/// configure providers that exist and are routable, and may only route to
/// models those providers actually serve.
fn validate_config(recipe: &LoadedRecipe, catalog: &Catalog, issues: &mut Vec<String>) {
    let slug = &recipe.dir_slug;

    for name in recipe.config.providers.keys() {
        match catalog.providers.get(name.as_str()) {
            None => issues.push(format!(
                "recipes/{slug}: bitrouter.yaml configures provider '{name}', which is not in the registry"
            )),
            Some(provider) if !provider.active => issues.push(format!(
                "recipes/{slug}: bitrouter.yaml configures provider '{name}', whose registry status is not active"
            )),
            Some(_) => {}
        }
    }

    for (provider_name, provider_config) in &recipe.config.providers {
        for model in &provider_config.models {
            let service_id = model.provider_model_id.as_deref().unwrap_or(&model.id);
            match catalog.providers.get(provider_name.as_str()) {
                Some(provider) if !provider.serves(service_id) => issues.push(format!(
                    "recipes/{slug}: template model '{}' routes to '{}' at provider '{provider_name}', which does not serve it",
                    model.id, service_id
                )),
                _ => {}
            }
        }
    }

    for (model, virtual_model) in &recipe.config.models {
        for endpoint in &virtual_model.endpoints {
            match catalog.providers.get(endpoint.provider.as_str()) {
                None => issues.push(format!(
                    "recipes/{slug}: model '{model}' routes to provider '{}', which is not in the registry",
                    endpoint.provider
                )),
                Some(provider) if !provider.serves(&endpoint.service_id) => {
                    issues.push(format!(
                        "recipes/{slug}: model '{model}' routes to '{}' at provider '{}', which does not serve it",
                        endpoint.service_id, endpoint.provider
                    ));
                }
                Some(_) => {}
            }
        }
    }

    for (tier, target) in &recipe.config.policy_table.tiers {
        if resolve_target(target, recipe, catalog).is_none() {
            issues.push(format!(
                "recipes/{slug}: policy_table tier '{tier}' routes to '{target}', which is neither a model defined in this config nor a model in the registry"
            ));
        }
    }

    if let Some(lock) = &recipe.policy_lock {
        if lock.document.lockfile_version != 2 {
            issues.push(format!(
                "recipes/{slug}: template policy lock is version {}, expected current version 2",
                lock.document.lockfile_version
            ));
        }
        for (policy_name, policy) in &lock.document.policies {
            for (tier, target) in &policy.tiers {
                if resolve_target(target, recipe, catalog).is_none() {
                    issues.push(format!(
                        "recipes/{slug}: policy '{policy_name}' tier '{tier}' routes to '{target}', which is not served by the template or registry"
                    ));
                }
            }
        }
    }

    for (preset_name, preset) in &recipe.config.presets {
        let Some(policy_name) = &preset.policy else {
            continue;
        };
        let policy_exists = recipe
            .policy_lock
            .as_ref()
            .is_some_and(|lock| lock.document.policies.contains_key(policy_name));
        if !policy_exists {
            issues.push(format!(
                "recipes/{slug}: template preset '{preset_name}' references missing policy '{policy_name}' in policy-lock.yaml"
            ));
        }
    }
}

/// Resolve a routing target to its canonical model id, or `None` when it
/// resolves to nothing the registry knows about. Accepts the three spellings a
/// `policy_table` tier may use: a virtual model defined in this config, an
/// explicit `provider:model` direct route, or a bare canonical id.
fn resolve_target(target: &str, recipe: &LoadedRecipe, catalog: &Catalog) -> Option<String> {
    if let Some(virtual_model) = recipe.config.models.get(target) {
        // A virtual model's own endpoints are validated separately; report the
        // canonical id of its first endpoint so the dist entry lists real models.
        let endpoint = virtual_model.endpoints.first()?;
        return catalog
            .providers
            .get(endpoint.provider.as_str())
            .and_then(|provider| provider.canonical(&endpoint.service_id));
    }
    if let Some((provider_name, service_id)) = target.split_once(':')
        && let Some(provider) = catalog.providers.get(provider_name)
        && let Some(canonical) = provider.canonical(service_id)
    {
        return Some(canonical);
    }
    catalog
        .canonical_models
        .contains(target)
        .then(|| target.to_string())
}

// --- dist rendering ----------------------------------------------------------

fn dist_value(recipe: &LoadedRecipe, catalog: &Catalog) -> Value {
    let meta = &recipe.meta;
    let slug = &recipe.dir_slug;

    let mut body = serde_json::Map::new();
    body.insert("en".to_string(), json!(recipe.body_en));
    if let Some(zh) = &recipe.body_zh {
        body.insert("zh".to_string(), json!(zh));
    }

    let mut object = serde_json::Map::from_iter([
        ("slug".to_string(), json!(slug)),
        ("template".to_string(), json!(meta.template)),
        (
            "template_url".to_string(),
            json!(format!("{TEMPLATE_BASE}/{}", meta.template)),
        ),
        ("title".to_string(), localized_value(&meta.title)),
        (
            "description".to_string(),
            localized_value(&meta.description),
        ),
        ("workflow".to_string(), json!(meta.workflow)),
        ("harness".to_string(), json!(meta.harness)),
        (
            "objectives".to_string(),
            json!(
                meta.objectives
                    .iter()
                    .map(Objective::as_str)
                    .collect::<Vec<_>>()
            ),
        ),
        ("updated_at".to_string(), json!(meta.updated_at)),
        (
            "providers".to_string(),
            json!(provider_requirements(recipe, catalog)),
        ),
        ("models".to_string(), json!(routed_models(recipe, catalog))),
        ("env".to_string(), json!(env_vars(&recipe.config_raw))),
        ("config".to_string(), json!(recipe.config_raw)),
        ("body".to_string(), Value::Object(body)),
        (
            "source_url".to_string(),
            json!(format!("{SOURCE_BASE}/{slug}")),
        ),
    ]);
    if let Some(policy_lock) = &recipe.policy_lock {
        object.insert("policy_lock".to_string(), json!(policy_lock.raw));
    }
    if let Some(evaluation) = &meta.evaluation {
        object.insert("evaluation".to_string(), evaluation_value(evaluation));
    }
    Value::Object(object)
}

fn localized_value(text: &Localized) -> Value {
    match &text.zh {
        Some(zh) => json!({ "en": text.en, "zh": zh }),
        None => json!({ "en": text.en }),
    }
}

fn evaluation_value(evaluation: &Evaluation) -> Value {
    let mut object = serde_json::Map::from_iter([
        ("eval".to_string(), json!(evaluation.eval)),
        ("harness".to_string(), json!(evaluation.harness)),
        ("measured_by".to_string(), json!(evaluation.measured_by)),
        ("as_of".to_string(), json!(evaluation.as_of)),
        ("runs".to_string(), json!(evaluation.runs)),
        (
            "artifacts".to_string(),
            json!({
                "config_sha256": evaluation.artifacts.config_sha256,
                "policy_lock_sha256": evaluation.artifacts.policy_lock_sha256,
            }),
        ),
        (
            "baseline".to_string(),
            measurement_value(&evaluation.baseline),
        ),
        ("recipe".to_string(), measurement_value(&evaluation.recipe)),
        (
            "delta".to_string(),
            delta_value(&evaluation.baseline, &evaluation.recipe),
        ),
    ]);
    if let Some(config) = &evaluation.config {
        object.insert("config".to_string(), json!(config));
    }
    if let Some(source_url) = &evaluation.source_url {
        object.insert("source_url".to_string(), json!(source_url));
    }
    Value::Object(object)
}

fn measurement_value(measurement: &Measurement) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(label) = &measurement.label {
        object.insert("label".to_string(), json!(label));
    }
    for (field, metric) in [
        ("accuracy", measurement.accuracy),
        ("cost_per_task", measurement.cost_per_task),
        ("time_per_task", measurement.time_per_task),
    ] {
        if let Some(metric) = metric {
            object.insert(field.to_string(), json!(metric));
        }
    }
    Value::Object(object)
}

/// The comparison the site renders, computed from the two measurements rather
/// than stored beside them. Accuracy moves in *points* (an 86.5 → 88.0 run
/// gained 1.5 points, not 1.7%); cost and time move in *percent*, which is how
/// a saving is actually quoted.
fn delta_value(baseline: &Measurement, recipe: &Measurement) -> Value {
    let mut object = serde_json::Map::new();
    if let (Some(before), Some(after)) = (baseline.accuracy, recipe.accuracy) {
        object.insert("accuracy_points".to_string(), json!(round1(after - before)));
    }
    for (field, before, after) in [
        (
            "cost_per_task_pct",
            baseline.cost_per_task,
            recipe.cost_per_task,
        ),
        (
            "time_per_task_pct",
            baseline.time_per_task,
            recipe.time_per_task,
        ),
    ] {
        if let (Some(before), Some(after)) = (before, after)
            && before != 0.0
        {
            object.insert(
                field.to_string(),
                json!(round1((after - before) / before * 100.0)),
            );
        }
    }
    Value::Object(object)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// The providers the config turns on, each with what a reader must supply
/// before it works — taken from the registry, so a provider that switches from
/// an API key to a sign-in updates every recipe that uses it.
fn provider_requirements(recipe: &LoadedRecipe, catalog: &Catalog) -> Value {
    let entries: Vec<Value> = recipe
        .config
        .providers
        .keys()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|name| {
            let requires = catalog
                .providers
                .get(name.as_str())
                .map(|provider| provider.requires.clone())
                .unwrap_or_default();
            json!({ "name": name, "requires": requires })
        })
        .collect();
    json!(entries)
}

/// Canonical ids of every model the config routes to, across virtual-model
/// endpoints and policy-table tiers — so the site can link a recipe to the
/// model pages it depends on without re-parsing the YAML.
fn routed_models(recipe: &LoadedRecipe, catalog: &Catalog) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (provider_name, provider_config) in &recipe.config.providers {
        for model in &provider_config.models {
            let service_id = model.provider_model_id.as_deref().unwrap_or(&model.id);
            let canonical = catalog
                .providers
                .get(provider_name.as_str())
                .and_then(|provider| provider.canonical(service_id))
                .unwrap_or_else(|| model.id.clone());
            out.insert(canonical);
        }
    }
    for virtual_model in recipe.config.models.values() {
        for endpoint in &virtual_model.endpoints {
            let canonical = catalog
                .providers
                .get(endpoint.provider.as_str())
                .and_then(|provider| provider.canonical(&endpoint.service_id))
                .unwrap_or_else(|| format!("{}:{}", endpoint.provider, endpoint.service_id));
            out.insert(canonical);
        }
    }
    for target in recipe.config.policy_table.tiers.values() {
        out.insert(resolve_target(target, recipe, catalog).unwrap_or_else(|| target.clone()));
    }
    if let Some(lock) = &recipe.policy_lock {
        for policy in lock.document.policies.values() {
            for target in policy.tiers.values() {
                out.insert(
                    resolve_target(target, recipe, catalog).unwrap_or_else(|| target.clone()),
                );
            }
        }
    }
    out
}

/// Environment variables the config interpolates — the "what you must set
/// before this works" list, read off the config instead of restated by hand.
/// Comment lines are skipped because the loader's own `${VAR}` substitution is
/// comment-aware: a commented-out example must not become a requirement.
fn env_vars(raw: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in raw.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else { break };
            let name = &after[..end];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                out.insert(name.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    out
}

// --- source schema -----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipeFile {
    /// Must equal the directory name.
    slug: String,
    status: RecipeStatus,
    title: Localized,
    description: Localized,
    /// Deployable artifact bundle under `templates/<template>/`.
    template: String,
    /// The task type this recipe routes (e.g. `coding`, `code-review`).
    workflow: String,
    /// Harnesses the recipe is written for.
    harness: Vec<String>,
    objectives: Vec<Objective>,
    updated_at: String,
    /// Required once `status: published` — see [`Evaluation`].
    #[serde(default)]
    evaluation: Option<Evaluation>,
}

#[derive(Debug, Deserialize)]
struct RecipePolicyLock {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    #[serde(default)]
    policies: std::collections::BTreeMap<String, RecipePolicy>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RecipePolicy {
    tiers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecipeStatus {
    /// Validated like any other recipe, but excluded from `dist/` — the state a
    /// recipe sits in until it has been measured.
    Draft,
    Published,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Objective {
    Cost,
    Latency,
    Accuracy,
}

impl Objective {
    fn as_str(&self) -> &'static str {
        match self {
            Objective::Cost => "cost",
            Objective::Latency => "latency",
            Objective::Accuracy => "accuracy",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Localized {
    en: String,
    /// Required on published recipes; the site has a Chinese locale and a
    /// half-translated catalog reads worse than an untranslated one.
    #[serde(default)]
    zh: Option<String>,
}

/// The measured run behind a recipe's claim. Provenance is mandatory for the
/// same reason it is on registry benchmarks: a score without the harness,
/// config, and author that produced it is not reproducible, and a cited
/// third-party number must never be mistaken for one we ran.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evaluation {
    /// The evaluation the numbers come from (e.g. `terminal-bench-2.1`).
    eval: String,
    /// Harness the run used. This is evidence provenance, not a routing key or
    /// a claim that the recipe is harness-specific.
    harness: String,
    /// Reasoning-effort / configuration label the run used.
    #[serde(default)]
    config: Option<String>,
    /// `bitrouter` for our own runs, otherwise the third-party source name.
    measured_by: String,
    /// Citation URL, required when `measured_by` is not `bitrouter`.
    #[serde(default)]
    source_url: Option<String>,
    /// Snapshot date (`YYYY-MM-DD`).
    as_of: String,
    /// How many times the evaluation was repeated.
    runs: u32,
    /// Exact deployable inputs used by the accepted runs. Publication fails if
    /// either digest differs from the recipe's current named template.
    artifacts: EvaluationArtifacts,
    /// What the recipe is compared against.
    baseline: Measurement,
    /// The same evaluation with the recipe's config applied.
    recipe: Measurement,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationArtifacts {
    config_sha256: String,
    policy_lock_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Measurement {
    /// What was run (e.g. `claude-opus-5, no routing`).
    #[serde(default)]
    label: Option<String>,
    /// Percent of tasks passed (`0..=100`).
    #[serde(default)]
    accuracy: Option<f64>,
    /// Average cost per task, in USD.
    #[serde(default)]
    cost_per_task: Option<f64>,
    /// Average time per task, in minutes.
    #[serde(default)]
    time_per_task: Option<f64>,
}

impl Measurement {
    /// Which metrics this side actually reports — used to prove both sides of a
    /// comparison measured the same things.
    fn metrics(&self) -> BTreeSet<&'static str> {
        let mut out = BTreeSet::new();
        if self.accuracy.is_some() {
            out.insert("accuracy");
        }
        if self.cost_per_task.is_some() {
            out.insert("cost_per_task");
        }
        if self.time_per_task.is_some() {
            out.insert("time_per_task");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bitrouter-recipes-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents.trim_start()).unwrap();
    }

    fn measurement(accuracy: Option<f64>, cost: Option<f64>, time: Option<f64>) -> Measurement {
        Measurement {
            label: None,
            accuracy,
            cost_per_task: cost,
            time_per_task: time,
        }
    }

    fn artifacts() -> EvaluationArtifacts {
        EvaluationArtifacts {
            config_sha256:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            policy_lock_sha256:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        }
    }

    #[test]
    fn content_digest_uses_sha256_over_exact_bytes() {
        assert_eq!(
            content_sha256("abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn recipe_loads_deployable_artifacts_from_its_template() {
        let root = test_root("template-source");
        write(
            &root,
            "recipes/auto-router/recipe.yaml",
            r#"
slug: auto-router
status: draft
title: { en: Auto router }
description: { en: Generic adaptive routing }
workflow: agentic
harness: [generic]
objectives: [cost]
updated_at: 2026-08-04
template: auto-router
"#,
        );
        write(&root, "recipes/auto-router/README.md", "Recipe body\n");
        write(
            &root,
            "templates/auto-router/bitrouter.yaml",
            "inherit_defaults: true\n",
        );
        write(
            &root,
            "templates/auto-router/policy-lock.yaml",
            "lockfileVersion: 2\npolicies: {}\n",
        );

        let result = load_recipes(&root);
        fs::remove_dir_all(&root).unwrap();
        let loaded = result.expect("recipe should load its named template");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].config_raw, "inherit_defaults: true\n");
        assert_eq!(
            loaded[0].policy_lock.as_ref().map(|lock| lock.raw.as_str()),
            Some("lockfileVersion: 2\npolicies: {}\n")
        );
    }

    #[test]
    fn routed_models_include_provider_models_selected_by_the_policy_lock() {
        let config_raw = include_str!("../../../templates/auto-router/bitrouter.yaml");
        let policy_lock = include_str!("../../../templates/auto-router/policy-lock.yaml");
        let config = bitrouter_sdk::config::parse_with(config_raw, |_| None)
            .expect("current auto-router config parses");
        let recipe = LoadedRecipe {
            dir_slug: "auto-router".into(),
            meta: RecipeFile {
                slug: "auto-router".into(),
                status: RecipeStatus::Published,
                title: Localized {
                    en: "Auto router".into(),
                    zh: None,
                },
                description: Localized {
                    en: "Generic adaptive routing".into(),
                    zh: None,
                },
                template: "auto-router".into(),
                workflow: "agentic".into(),
                harness: vec!["generic".into()],
                objectives: vec![Objective::Cost],
                updated_at: "2026-08-04".into(),
                evaluation: None,
            },
            config_raw: config_raw.into(),
            config,
            policy_lock: Some(LoadedPolicyLock {
                raw: policy_lock.into(),
                document: serde_saphyr::from_str(policy_lock)
                    .expect("current auto-router lock parses"),
            }),
            body_en: String::new(),
            body_zh: None,
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = crate::registry::catalog(&root).expect("current registry loads");

        assert_eq!(
            routed_models(&recipe, &catalog),
            BTreeSet::from([
                "deepseek/deepseek-v4-pro".to_string(),
                "moonshotai/kimi-k3".to_string(),
                "openai/gpt-5.6-sol".to_string(),
            ])
        );
    }

    #[test]
    fn template_preset_policy_must_exist_in_the_sibling_lock() {
        let config_raw = r#"
presets:
  auto:
    model: openai/gpt-5.6-sol
    policy: auto
"#;
        let policy_lock = "lockfileVersion: 2\npolicies: {}\n";
        let recipe = LoadedRecipe {
            dir_slug: "auto-router".into(),
            meta: RecipeFile {
                slug: "auto-router".into(),
                status: RecipeStatus::Draft,
                title: Localized {
                    en: "Auto router".into(),
                    zh: None,
                },
                description: Localized {
                    en: "Generic adaptive routing".into(),
                    zh: None,
                },
                template: "auto-router".into(),
                workflow: "agentic".into(),
                harness: vec!["generic".into()],
                objectives: vec![Objective::Cost],
                updated_at: "2026-08-04".into(),
                evaluation: None,
            },
            config_raw: config_raw.into(),
            config: bitrouter_sdk::config::parse_with(config_raw, |_| None)
                .expect("fixture config parses"),
            policy_lock: Some(LoadedPolicyLock {
                raw: policy_lock.into(),
                document: serde_saphyr::from_str(policy_lock).expect("fixture lock parses"),
            }),
            body_en: String::new(),
            body_zh: None,
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = crate::registry::catalog(&root).expect("current registry loads");
        let mut issues = Vec::new();

        validate_config(&recipe, &catalog, &mut issues);

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("preset 'auto' references missing policy 'auto'"));
    }

    #[test]
    fn delta_reports_accuracy_in_points_and_cost_in_percent() {
        let baseline = measurement(Some(86.5), Some(1.20), Some(4.6));
        let recipe = measurement(Some(88.0), Some(0.74), Some(4.3));
        let delta = delta_value(&baseline, &recipe);
        assert_eq!(delta["accuracy_points"], json!(1.5));
        assert_eq!(delta["cost_per_task_pct"], json!(-38.3));
        assert_eq!(delta["time_per_task_pct"], json!(-6.5));
    }

    #[test]
    fn delta_omits_metrics_measured_on_only_one_side() {
        let delta = delta_value(
            &measurement(Some(80.0), Some(1.0), None),
            &measurement(None, Some(0.5), Some(2.0)),
        );
        let object = delta.as_object().expect("delta is an object");
        assert_eq!(object.keys().collect::<Vec<_>>(), vec!["cost_per_task_pct"]);
    }

    #[test]
    fn metrics_mismatch_is_an_issue() {
        let evaluation = Evaluation {
            eval: "terminal-bench-2.1".into(),
            harness: "claude-code".into(),
            config: None,
            measured_by: "bitrouter".into(),
            source_url: None,
            as_of: "2026-07-25".into(),
            runs: 3,
            artifacts: artifacts(),
            baseline: measurement(Some(80.0), Some(1.0), None),
            recipe: measurement(Some(82.0), None, None),
        };
        let mut issues = Vec::new();
        validate_evaluation("x", &evaluation, &mut issues);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("cost_per_task on only one of baseline / recipe"));
    }

    #[test]
    fn third_party_measurement_requires_a_citation() {
        let evaluation = Evaluation {
            eval: "terminal-bench-2.1".into(),
            harness: "codex".into(),
            config: None,
            measured_by: "artificial-analysis".into(),
            source_url: None,
            as_of: "2026-07-25".into(),
            runs: 1,
            artifacts: artifacts(),
            baseline: measurement(None, Some(1.0), None),
            recipe: measurement(None, Some(0.5), None),
        };
        let mut issues = Vec::new();
        validate_evaluation("x", &evaluation, &mut issues);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("source_url is required"));
    }

    #[test]
    fn evaluation_harness_is_evidence_not_a_routing_identity() {
        let evaluation = Evaluation {
            eval: "terminal-bench-2.1-short13".into(),
            harness: "terminus-2".into(),
            config: Some("frozen-r3".into()),
            measured_by: "bitrouter".into(),
            source_url: Some("https://github.com/bitrouter/bitrouter/pull/768".into()),
            as_of: "2026-08-04".into(),
            runs: 2,
            artifacts: artifacts(),
            baseline: measurement(Some(80.77), Some(0.429854), None),
            recipe: measurement(Some(84.62), Some(0.359072), None),
        };
        let mut issues = Vec::new();

        validate_evaluation("auto-router", &evaluation, &mut issues);

        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn evaluation_provenance_names_the_exact_template_artifacts() {
        let raw = r#"
eval: terminal-bench-2.1-short13
harness: terminus-2
measured_by: bitrouter
as_of: 2026-08-04
runs: 2
artifacts:
  config_sha256: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  policy_lock_sha256: sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
baseline: { cost_per_task: 1.0 }
recipe: { cost_per_task: 0.8 }
"#;

        let result = serde_saphyr::from_str::<Evaluation>(raw);

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn published_measurement_must_match_the_current_template_bytes() {
        let config_raw = include_str!("../../../templates/auto-router/bitrouter.yaml");
        let policy_lock = include_str!("../../../templates/auto-router/policy-lock.yaml");
        let evaluation = Evaluation {
            eval: "terminal-bench-2.1-short13".into(),
            harness: "terminus-2".into(),
            config: Some("frozen-r3".into()),
            measured_by: "bitrouter".into(),
            source_url: Some("https://github.com/bitrouter/bitrouter/pull/768".into()),
            as_of: "2026-08-04".into(),
            runs: 2,
            artifacts: artifacts(),
            baseline: measurement(Some(80.77), Some(0.429854), None),
            recipe: measurement(Some(84.62), Some(0.359072), None),
        };
        let recipe = LoadedRecipe {
            dir_slug: "auto-router".into(),
            meta: RecipeFile {
                slug: "auto-router".into(),
                status: RecipeStatus::Published,
                title: Localized {
                    en: "Auto router".into(),
                    zh: Some("自动路由".into()),
                },
                description: Localized {
                    en: "Generic adaptive routing".into(),
                    zh: Some("通用自适应路由".into()),
                },
                template: "auto-router".into(),
                workflow: "agentic".into(),
                harness: vec!["generic".into()],
                objectives: vec![Objective::Cost],
                updated_at: "2026-08-04".into(),
                evaluation: Some(evaluation),
            },
            config_raw: config_raw.into(),
            config: bitrouter_sdk::config::parse_with(config_raw, |_| None)
                .expect("current auto-router config parses"),
            policy_lock: Some(LoadedPolicyLock {
                raw: policy_lock.into(),
                document: serde_saphyr::from_str(policy_lock)
                    .expect("current auto-router lock parses"),
            }),
            body_en: String::new(),
            body_zh: Some(String::new()),
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = crate::registry::catalog(&root).expect("current registry loads");
        let mut issues = Vec::new();
        let mut advisories = Vec::new();

        validate_recipe(&recipe, &catalog, &mut issues, &mut advisories);

        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues.iter().any(|issue| issue.contains("config_sha256")));
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("policy_lock_sha256"))
        );
    }

    #[test]
    fn env_vars_ignores_commented_examples() {
        let raw = "\
providers:
  openai:
    api_key: \"${OPENAI_API_KEY}\"
  # opencode-go: { api_key: \"${OPENCODE_GO_KEY_A}\" }
";
        assert_eq!(
            env_vars(raw),
            BTreeSet::from(["OPENAI_API_KEY".to_string()])
        );
    }

    #[test]
    fn env_vars_collects_multiple_refs_on_one_line() {
        let raw = "url: \"https://${HOST}/${PATH_SEGMENT}\"";
        assert_eq!(
            env_vars(raw),
            BTreeSet::from(["HOST".to_string(), "PATH_SEGMENT".to_string()])
        );
    }
}
