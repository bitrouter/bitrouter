//! Workflow recipes: `recipes/<slug>/` source → committed
//! `dist/recipes/index.json`.
//!
//! A recipe is a drop-in `bitrouter.yaml` for one workflow, plus the measured
//! result of running it against a baseline. Two rules shape everything here:
//!
//! 1. **Anything derivable from the config is derived, never restated.** The
//!    providers, models, and environment variables a recipe needs are extracted
//!    from its `bitrouter.yaml`, so metadata cannot drift from the config it
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

use crate::registry::{Catalog, serialize_data, valid_slug, valid_yyyy_mm_dd};

/// Where a recipe's source directory is browsable — recorded per entry so the
/// site can link "view source" without hardcoding the repo layout.
const SOURCE_BASE: &str = "https://github.com/bitrouter/bitrouter/tree/main/recipes";

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
    policy_lock: Option<String>,
    body_en: String,
    body_zh: Option<String>,
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
        out.push(load_recipe(&path)?);
    }
    Ok(out)
}

fn load_recipe(dir: &Path) -> Result<LoadedRecipe> {
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

    let config_path = dir.join("bitrouter.yaml");
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
        policy_lock: read_optional(&dir.join("policy-lock.yaml"))?,
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
        Some(evaluation) => validate_evaluation(slug, meta, evaluation, issues),
        None if published => issues.push(format!(
            "recipes/{slug}: status is published but no evaluation block is present - a recipe is measured before it is released"
        )),
        None => {}
    }

    validate_config(recipe, catalog, issues);
}

fn validate_evaluation(
    slug: &str,
    meta: &RecipeFile,
    evaluation: &Evaluation,
    issues: &mut Vec<String>,
) {
    if evaluation.eval.trim().is_empty() {
        issues.push(format!("recipes/{slug}: evaluation.eval is empty"));
    }
    if !meta.harness.contains(&evaluation.harness) {
        issues.push(format!(
            "recipes/{slug}: evaluation.harness '{}' is not one of the recipe's harnesses",
            evaluation.harness
        ));
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

    let mut entry = json!({
        "slug": slug,
        "title": localized_value(&meta.title),
        "description": localized_value(&meta.description),
        "workflow": meta.workflow,
        "harness": meta.harness,
        "objectives": meta.objectives.iter().map(Objective::as_str).collect::<Vec<_>>(),
        "updated_at": meta.updated_at,
        "providers": provider_requirements(recipe, catalog),
        "models": routed_models(recipe, catalog),
        "env": env_vars(&recipe.config_raw),
        "config": recipe.config_raw,
        "body": {
            "en": recipe.body_en,
        },
        "source_url": format!("{SOURCE_BASE}/{slug}"),
    });

    let object = entry.as_object_mut().expect("json! built an object");
    if let Some(policy_lock) = &recipe.policy_lock {
        object.insert("policy_lock".to_string(), json!(policy_lock));
    }
    if let Some(zh) = &recipe.body_zh {
        object["body"]["zh"] = json!(zh);
    }
    if let Some(evaluation) = &meta.evaluation {
        object.insert("evaluation".to_string(), evaluation_value(evaluation));
    }
    entry
}

fn localized_value(text: &Localized) -> Value {
    match &text.zh {
        Some(zh) => json!({ "en": text.en, "zh": zh }),
        None => json!({ "en": text.en }),
    }
}

fn evaluation_value(evaluation: &Evaluation) -> Value {
    let mut value = json!({
        "eval": evaluation.eval,
        "harness": evaluation.harness,
        "measured_by": evaluation.measured_by,
        "as_of": evaluation.as_of,
        "runs": evaluation.runs,
        "baseline": measurement_value(&evaluation.baseline),
        "recipe": measurement_value(&evaluation.recipe),
        "delta": delta_value(&evaluation.baseline, &evaluation.recipe),
    });
    let object = value.as_object_mut().expect("json! built an object");
    if let Some(config) = &evaluation.config {
        object.insert("config".to_string(), json!(config));
    }
    if let Some(source_url) = &evaluation.source_url {
        object.insert("source_url".to_string(), json!(source_url));
    }
    value
}

fn measurement_value(measurement: &Measurement) -> Value {
    let mut value = json!({});
    let object = value.as_object_mut().expect("json! built an object");
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
    value
}

/// The comparison the site renders, computed from the two measurements rather
/// than stored beside them. Accuracy moves in *points* (an 86.5 → 88.0 run
/// gained 1.5 points, not 1.7%); cost and time move in *percent*, which is how
/// a saving is actually quoted.
fn delta_value(baseline: &Measurement, recipe: &Measurement) -> Value {
    let mut value = json!({});
    let object = value.as_object_mut().expect("json! built an object");
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
    value
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
    /// Harness the run used — must be one of the recipe's own `harness` list.
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
    /// What the recipe is compared against.
    baseline: Measurement,
    /// The same evaluation with the recipe's config applied.
    recipe: Measurement,
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
    use super::*;

    fn measurement(accuracy: Option<f64>, cost: Option<f64>, time: Option<f64>) -> Measurement {
        Measurement {
            label: None,
            accuracy,
            cost_per_task: cost,
            time_per_task: time,
        }
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
        let meta = RecipeFile {
            slug: "x".into(),
            status: RecipeStatus::Published,
            title: Localized {
                en: "x".into(),
                zh: Some("x".into()),
            },
            description: Localized {
                en: "x".into(),
                zh: Some("x".into()),
            },
            workflow: "coding".into(),
            harness: vec!["claude-code".into()],
            objectives: vec![Objective::Cost],
            updated_at: "2026-07-25".into(),
            evaluation: None,
        };
        let evaluation = Evaluation {
            eval: "terminal-bench-2.1".into(),
            harness: "claude-code".into(),
            config: None,
            measured_by: "bitrouter".into(),
            source_url: None,
            as_of: "2026-07-25".into(),
            runs: 3,
            baseline: measurement(Some(80.0), Some(1.0), None),
            recipe: measurement(Some(82.0), None, None),
        };
        let mut issues = Vec::new();
        validate_evaluation("x", &meta, &evaluation, &mut issues);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("cost_per_task on only one of baseline / recipe"));
    }

    #[test]
    fn third_party_measurement_requires_a_citation() {
        let meta = RecipeFile {
            slug: "x".into(),
            status: RecipeStatus::Published,
            title: Localized {
                en: "x".into(),
                zh: None,
            },
            description: Localized {
                en: "x".into(),
                zh: None,
            },
            workflow: "coding".into(),
            harness: vec!["codex".into()],
            objectives: vec![Objective::Cost],
            updated_at: "2026-07-25".into(),
            evaluation: None,
        };
        let evaluation = Evaluation {
            eval: "terminal-bench-2.1".into(),
            harness: "codex".into(),
            config: None,
            measured_by: "artificial-analysis".into(),
            source_url: None,
            as_of: "2026-07-25".into(),
            runs: 1,
            baseline: measurement(None, Some(1.0), None),
            recipe: measurement(None, Some(0.5), None),
        };
        let mut issues = Vec::new();
        validate_evaluation("x", &meta, &evaluation, &mut issues);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("source_url is required"));
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
