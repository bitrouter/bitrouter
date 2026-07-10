use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub fn validate(root: &Path) -> Result<()> {
    let loaded = load_registry(root)?;
    let advisories = validate_loaded(&loaded)?;
    println!(
        "registry valid: {} canonical models, {} providers",
        loaded.models().count(),
        loaded.providers.len()
    );
    if !advisories.is_empty() {
        println!(
            "note: {} provider model(s) not in curated registry/models (BYOK / BYO-subscription extras):",
            advisories.len()
        );
        for advisory in &advisories {
            println!("  - {advisory}");
        }
    }
    Ok(())
}

pub fn build(root: &Path, check: bool) -> Result<()> {
    let artifacts = build_artifacts(root)?;
    let providers_path = dist_dir(root).join("providers.json");
    let models_path = dist_dir(root).join("models.json");
    if check {
        let current_providers = fs::read_to_string(&providers_path)
            .with_context(|| format!("reading {}", providers_path.display()))?;
        let current_models = fs::read_to_string(&models_path)
            .with_context(|| format!("reading {}", models_path.display()))?;
        if current_providers != artifacts.providers || current_models != artifacts.models {
            bail!(
                "registry dist is stale - run `cargo run -p dist-helper -- registry build` and commit dist/registry"
            );
        }
        println!(
            "registry dist is up to date: {} providers, {} canonical models",
            artifacts.provider_count, artifacts.model_count
        );
        return Ok(());
    }
    fs::create_dir_all(dist_dir(root))
        .with_context(|| format!("creating {}", dist_dir(root).display()))?;
    fs::write(&providers_path, artifacts.providers)
        .with_context(|| format!("writing {}", providers_path.display()))?;
    fs::write(&models_path, artifacts.models)
        .with_context(|| format!("writing {}", models_path.display()))?;
    println!(
        "wrote dist/registry/providers.json - {} providers; dist/registry/models.json - {} canonical models",
        artifacts.provider_count, artifacts.model_count
    );
    Ok(())
}

pub async fn sync(root: &Path, write: bool) -> Result<()> {
    let mut loaded = load_registry(root)?;
    validate_loaded(&loaded)?;
    sync_models_dev_loaded(root, &loaded, write).await?;
    if write {
        loaded = load_registry(root)?;
        validate_loaded(&loaded)?;
    }
    sync_v1_models_loaded(root, &loaded, write).await?;
    if write {
        validate(root)?;
        println!("\nsynced registry source data");
    }
    Ok(())
}

async fn sync_models_dev_loaded(_root: &Path, loaded: &LoadedRegistry, write: bool) -> Result<()> {
    if !loaded.providers.iter().any(|provider| {
        provider
            .data
            .auto_sync
            .as_ref()
            .is_some_and(|sync| sync.feed == AutoSyncFeed::ModelsDev && sync_writes_models(sync))
    }) {
        println!(
            "\nregistry sync - {} - keyless models.dev catalog attach",
            if write { "WRITE" } else { "dry-run" }
        );
        println!("attach 0 model(s) across 0 provider(s)");
        println!("  (no models.dev providers configured)");
        if !write {
            println!("\n(dry run - pass --write to apply)");
        }
        return Ok(());
    }
    let catalog = load_models_dev_catalog().await?;
    let resolve = canonical_resolver(loaded.models().map(|m| m.id.as_str()));
    let providers_by_name: HashMap<&str, &LoadedProvider> = loaded
        .providers
        .iter()
        .map(|p| (p.data.name.as_str(), p))
        .collect();
    let mut attaches: BTreeMap<String, Vec<ProviderModel>> = BTreeMap::new();

    for provider in &loaded.providers {
        let Some(sync) = &provider.data.auto_sync else {
            continue;
        };
        if sync.feed != AutoSyncFeed::ModelsDev {
            continue;
        }
        if !sync_writes_models(sync) {
            continue;
        }
        let key = sync.key.as_deref().unwrap_or(&provider.data.name);
        let Some(models) = catalog.providers.get(key) else {
            eprintln!(
                "  {} (models.dev:{key}): no catalog - skipped",
                provider.data.name
            );
            continue;
        };
        let have: HashSet<&str> = provider.data.models.iter().map(|m| m.id.as_str()).collect();
        let mut staged = HashSet::new();
        let subscription = provider.data.billing == Billing::Subscription;
        for (model_id, model) in &models.models {
            let Some(canonical_id) = resolve(model_id) else {
                continue;
            };
            if have.contains(canonical_id.as_str()) || !staged.insert(canonical_id.clone()) {
                continue;
            }
            let pricing = if subscription {
                None
            } else {
                pricing_from_cost(model.cost.as_ref())
            };
            attaches
                .entry(provider.data.name.clone())
                .or_default()
                .push(ProviderModel {
                    id: canonical_id,
                    provider_model_id: model_id.clone(),
                    api_protocol: None,
                    pricing,
                    rate_limits: None,
                    capabilities: Vec::new(),
                    deprecation_date: None,
                });
        }
    }

    let total: usize = attaches.values().map(Vec::len).sum();
    println!(
        "\nregistry sync - {} - keyless models.dev catalog attach",
        if write { "WRITE" } else { "dry-run" }
    );
    println!(
        "attach {total} model(s) across {} provider(s)",
        attaches.len()
    );
    for (provider, models) in &attaches {
        for model in models {
            println!(
                "  + {provider} <- {} ({})",
                model.id, model.provider_model_id
            );
        }
    }
    if total == 0 {
        println!("  (nothing to attach)");
    }
    if !write {
        println!("\n(dry run - pass --write to apply)");
        return Ok(());
    }

    for (provider, adds) in attaches {
        let loaded_provider = providers_by_name
            .get(provider.as_str())
            .context("sync plan referenced an unknown provider")?;
        append_models_to_provider(&loaded_provider.path, &adds)?;
    }
    Ok(())
}

async fn sync_v1_models_loaded(root: &Path, loaded: &LoadedRegistry, write: bool) -> Result<()> {
    let resolve = canonical_resolver(loaded.models().map(|m| m.id.as_str()));
    let providers_by_name: HashMap<&str, &LoadedProvider> = loaded
        .providers
        .iter()
        .map(|p| (p.data.name.as_str(), p))
        .collect();
    let mut attaches: BTreeMap<String, Vec<ProviderModel>> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut skipped = Vec::new();

    for provider in &loaded.providers {
        let Some(sync) = &provider.data.auto_sync else {
            continue;
        };
        if sync.feed != AutoSyncFeed::V1Models || !sync_writes_models(sync) {
            continue;
        }
        let Some(url) = v1_models_url(&provider.data) else {
            skipped.push(format!(
                "{}: no auto_sync.url or api_base for v1_models",
                provider.data.name
            ));
            continue;
        };
        let body = fetch_v1_models(&url, v1_auth_headers(&provider.data))
            .await
            .with_context(|| format!("syncing {} from {url}", provider.data.name))?;
        let plan = v1_models_plan_for_provider(&provider.data, &body, &resolve)
            .with_context(|| format!("planning v1_models sync for {}", provider.data.name))?;
        if !plan.adds.is_empty() {
            attaches.insert(provider.data.name.clone(), plan.adds);
        }
        if !plan.unresolved.is_empty() {
            unresolved.insert(provider.data.name.clone(), plan.unresolved);
        }
    }

    let total: usize = attaches.values().map(Vec::len).sum();
    println!(
        "\nregistry sync - {} - OpenAI-compatible /models attach",
        if write { "WRITE" } else { "dry-run" }
    );
    println!(
        "attach {total} model(s) across {} provider(s)",
        attaches.len()
    );
    for (provider, models) in &attaches {
        for model in models {
            println!(
                "  + {provider} <- {} ({})",
                model.id, model.provider_model_id
            );
        }
    }
    if !unresolved.is_empty() {
        println!("unresolved upstream model ids (no canonical model match):");
        for (provider, models) in &unresolved {
            for model in models {
                println!("  ? {provider} <- {model}");
            }
        }
    }
    for item in &skipped {
        eprintln!("  {item} - skipped");
    }
    if total == 0 {
        println!("  (nothing to attach)");
    }
    if !write {
        println!("\n(dry run - pass --write to apply)");
        return Ok(());
    }

    for (provider, adds) in attaches {
        let loaded_provider = providers_by_name
            .get(provider.as_str())
            .context("sync plan referenced an unknown provider")?;
        append_models_to_provider(&loaded_provider.path, &adds)?;
    }
    validate(root)?;
    Ok(())
}

struct V1ModelsPlan {
    adds: Vec<ProviderModel>,
    unresolved: Vec<String>,
}

fn v1_models_plan_for_provider(
    provider: &ProviderFile,
    body: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Result<V1ModelsPlan> {
    let catalog: V1ModelsResponse =
        serde_json::from_str(body).context("parsing OpenAI-compatible /models response")?;
    let have: HashSet<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
    let mut staged = HashSet::new();
    let mut unresolved_seen = HashSet::new();
    let mut adds = Vec::new();
    let mut unresolved = Vec::new();

    for model in catalog.data {
        let Some(canonical_id) = resolve(&model.id) else {
            if unresolved_seen.insert(model.id.clone()) {
                unresolved.push(model.id);
            }
            continue;
        };
        if have.contains(canonical_id.as_str()) || !staged.insert(canonical_id.clone()) {
            continue;
        }
        let pricing = match provider.billing {
            Billing::Subscription => None,
            Billing::UsageToken => match model.pricing {
                Some(pricing) => Some(pricing),
                None => continue,
            },
        };
        adds.push(ProviderModel {
            id: canonical_id,
            provider_model_id: model.id,
            api_protocol: None,
            pricing,
            rate_limits: None,
            capabilities: Vec::new(),
            deprecation_date: None,
        });
    }
    Ok(V1ModelsPlan { adds, unresolved })
}

fn v1_models_url(provider: &ProviderFile) -> Option<String> {
    let sync = provider.auto_sync.as_ref()?;
    let raw = sync.url.as_deref().or(provider.api_base.as_deref())?;
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/models") {
        Some(trimmed.to_string())
    } else {
        Some(format!("{trimmed}/models"))
    }
}

fn v1_auth_headers(_provider: &ProviderFile) -> Vec<(String, String)> {
    Vec::new()
}

async fn fetch_v1_models(url: &str, headers: Vec<(String, String)>) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("dist-helper/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building v1_models HTTP client")?;
    let mut request = client.get(url);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    request
        .send()
        .await
        .context("fetching /models catalog")?
        .error_for_status()
        .context("/models returned an error")?
        .text()
        .await
        .context("reading /models response")
}

#[derive(Debug, Deserialize)]
struct V1ModelsResponse {
    data: Vec<V1Model>,
}

#[derive(Debug, Deserialize)]
struct V1Model {
    id: String,
    #[serde(default)]
    pricing: Option<ModelPricing>,
}

pub fn agentic_prompt(root: &Path) -> Result<String> {
    let loaded = load_registry(root)?;
    validate_loaded(&loaded)?;
    let providers: Vec<_> = loaded
        .providers
        .iter()
        .filter(|provider| {
            provider
                .data
                .auto_sync
                .as_ref()
                .is_some_and(|sync| sync.feed == AutoSyncFeed::Agentic)
        })
        .collect();

    let mut out = String::new();
    writeln!(
        out,
        "You are running inside the bitrouter OSS repository.\n"
    )?;
    writeln!(out, "Goal:")?;
    writeln!(
        out,
        "Update the public model registry source files for the agentic-sync providers listed below.\n"
    )?;
    writeln!(out, "Hard rules:")?;
    writeln!(
        out,
        "- Only edit files under `registry/providers/` and `registry/models/`."
    )?;
    writeln!(out, "- Do not edit `dist/`; it will be regenerated later.")?;
    writeln!(
        out,
        "- Do not edit Rust code, workflows, docs, Cargo files, or unrelated files."
    )?;
    writeln!(
        out,
        "- This is not curation. Include all public production models supported by each provider."
    )?;
    writeln!(
        out,
        "- Preserve existing provider IDs and canonical model IDs."
    )?;
    writeln!(
        out,
        "- Do not remove or edit provider `auto_sync` configuration."
    )?;
    writeln!(
        out,
        "- Do not delete existing model entries unless the linked official source clearly says the model is removed or unavailable."
    )?;
    writeln!(
        out,
        "- If a model is uncertain, keep it and mention the uncertainty in your final response.\n"
    )?;
    writeln!(
        out,
        "- If the listed URLs are unreachable, make no model catalog changes for that provider and report it as skipped."
    )?;
    writeln!(
        out,
        "- Do not revert existing worktree changes; only make the required registry catalog edits."
    )?;
    writeln!(
        out,
        "- Do not infer provider catalog changes from `dist/` artifacts or helper source code.\n"
    )?;
    writeln!(
        out,
        "- Do not use YAML serializers, formatters, or full-file rewrites. Preserve comments, ordering, quoting, and indentation; edit the smallest necessary YAML ranges."
    )?;
    writeln!(
        out,
        "- Do not omit confirmed public models just to keep the diff small. Large catalog updates are allowed when the linked source supports them, but avoid unrelated formatting churn."
    )?;
    writeln!(
        out,
        "- The current source model count is context only, not a limit. Add every public production model from the linked source that maps to an existing canonical model ID.\n"
    )?;
    writeln!(out, "Source reading rules:")?;
    writeln!(
        out,
        "- The workflow installs `curl` and `rg`; use them before other fetch/parsing tools."
    )?;
    writeln!(
        out,
        "- For each source URL, first run `mkdir -p target/agentic-sync`, fetch the full primary document with `curl -sS -L <url> -o target/agentic-sync/<provider>-<n>.html`, then inspect that saved file with `rg`."
    )?;
    writeln!(
        out,
        "- Raw HTML or rendered app HTML is still readable source material, not a reason to skip a provider."
    )?;
    writeln!(
        out,
        "- Do not use truncated output, first lines, or `head` output to conclude that a catalog is missing. Fetch the full response or save it to a temporary file before deciding."
    )?;
    writeln!(
        out,
        "- If a page is long, noisy, or rendered by a frontend framework, use generic extraction strategies: convert to text/Markdown with an available reader tool, parse visible text, search embedded JSON, or inspect repeated model/pricing records in the full page."
    )?;
    writeln!(
        out,
        "- Do not print large raw HTML, YAML, or JSON files to stdout. Use targeted extraction commands such as `rg -o`, counts, or small scripts that emit compact summaries."
    )?;
    writeln!(
        out,
        "- Do not fetch `_next/`, static assets, JavaScript chunks, CSS, fonts, or images unless the saved primary document and a text/Markdown fallback both lack catalog data."
    )?;
    writeln!(
        out,
        "- Only report a source as unreadable after full-page retrieval and at least one fallback extraction method both fail.\n"
    )?;
    writeln!(out, "Providers to sync:\n")?;
    if providers.is_empty() {
        writeln!(
            out,
            "(No `auto_sync.feed: agentic` providers are configured.)\n"
        )?;
    } else {
        for provider in providers {
            render_agentic_provider(root, provider, &mut out)?;
        }
        writeln!(out)?;
    }
    writeln!(out, "Canonical model source:")?;
    writeln!(
        out,
        "- `registry/models/<vendor>.yaml` is the CURATED catalog of models BitRouter blesses by default (one file per vendor, a YAML sequence). It is maintainer-owned: do NOT create, edit, or delete canonical model entries during sync — only provider files change here."
    )?;
    writeln!(
        out,
        "- When a provider serves a model already in the catalog, reuse its exact canonical ID so it links. Search existing IDs with:"
    )?;
    writeln!(out, "  `rg -n \"^- id: \" registry/models`")?;
    writeln!(
        out,
        "- A provider MAY also serve models that are not in the catalog; give them a lowercase `<org>/<model>` ID. These are allowed (reported as non-failing advisories), so do not add a canonical entry for them.\n"
    )?;
    writeln!(out, "Provider model rules:")?;
    writeln!(
        out,
        "- Set `provider_model_id` to the exact upstream model id."
    )?;
    writeln!(
        out,
        "- Set `api_protocol` only when the model differs from provider-level defaults."
    )?;
    writeln!(
        out,
        "- Add `capabilities` only when clearly documented: tools, reasoning, structured_outputs, image_input, audio_input, video_input, file_input, image_output, audio_output, web_search, logprobs."
    )?;
    writeln!(
        out,
        "- Only modify data classes listed in the provider `writes` line."
    )?;
    writeln!(
        out,
        "- Treat `writes: models` as permission to add or remove provider model entries. A newly added provider model entry may include its own `pricing` when the linked source documents it."
    )?;
    writeln!(
        out,
        "- Treat `writes: pricing` as permission to update `pricing` on pre-existing provider model entries."
    )?;
    writeln!(
        out,
        "- When `writes` includes `pricing`, re-check pricing for every provider model against the linked source. Update confirmed changes; leave pricing unchanged only when it cannot be confirmed."
    )?;
    writeln!(
        out,
        "- If `writes` does not include `pricing`, preserve `pricing` in all pre-existing model entries exactly; do not recalculate, normalize, or remove it."
    )?;
    writeln!(
        out,
        "- For subscription providers, do not invent token pricing.\n"
    )?;
    writeln!(out, "Pricing unit rules:")?;
    writeln!(
        out,
        "- Registry pricing values are USD per 1 million tokens unless the schema field explicitly says otherwise."
    )?;
    writeln!(
        out,
        "- Credits, points, coins, or other provider-internal units are not USD. Find and cite the provider's conversion to USD before using them."
    )?;
    writeln!(
        out,
        "- If the source only exposes provider-internal units and no USD conversion can be confirmed, do not copy provider-internal unit numbers into pricing. Preserve existing pricing for existing entries; skip new usage-token models whose pricing cannot be converted and report them as uncertain."
    )?;
    writeln!(
        out,
        "- Before broad pricing rewrites, compare at least one existing model's current registry price against the source number. Large uniform multipliers usually mean a unit conversion is missing; re-check the source instead of applying the raw numbers.\n"
    )?;
    writeln!(out, "Validation:")?;
    writeln!(
        out,
        "Always run this command before your final response, even if no source files changed:"
    )?;
    writeln!(out, "`cargo run -p dist-helper -- registry validate`\n")?;
    writeln!(
        out,
        "If validation fails, fix the YAML and rerun exactly the same command."
    )?;
    writeln!(
        out,
        "Advisory notes about provider models not in the curated catalog are expected and are NOT failures — do not add canonical entries to silence them."
    )?;
    writeln!(out, "Do not run `cargo run -p dist-helper -- check`.")?;
    writeln!(
        out,
        "Do not edit `dist/`; the workflow regenerates dist after this agent exits.\n"
    )?;
    writeln!(out, "Final response must summarize:")?;
    writeln!(out, "- providers changed")?;
    writeln!(out, "- models added or updated")?;
    writeln!(
        out,
        "- models skipped because canonical mapping or facts were uncertain"
    )?;
    writeln!(
        out,
        "- pricing units and conversions used, especially for credits/points/coins"
    )?;
    writeln!(out, "- validation result")?;
    writeln!(
        out,
        "- include the exact `registry valid:` output line from validation"
    )?;
    Ok(out)
}

fn render_agentic_provider(root: &Path, provider: &LoadedProvider, out: &mut String) -> Result<()> {
    let data = &provider.data;
    let sync = data
        .auto_sync
        .as_ref()
        .context("agentic provider missing auto_sync")?;
    writeln!(
        out,
        "- `{}` (`{}`)",
        data.name,
        slash_path(provider.path.strip_prefix(root).unwrap_or(&provider.path))
    )?;
    if let Some(display_name) = &data.display_name {
        writeln!(out, "  - display_name: {display_name}")?;
    }
    writeln!(
        out,
        "  - status: {:?}; billing: {:?}; access: {:?}; existing_model_count: {} (current source count, not a limit)",
        data.status,
        data.billing,
        data.access,
        data.models.len()
    )?;
    if let Some(api_base) = &data.api_base {
        writeln!(out, "  - api_base: {api_base}")?;
    }
    if !sync.writes.as_ref().is_none_or(Vec::is_empty) {
        let writes: Vec<_> = sync
            .writes
            .as_ref()
            .into_iter()
            .flatten()
            .map(|write| write.source_key())
            .collect();
        writeln!(out, "  - writes: {}", writes.join(", "))?;
    } else {
        writeln!(out, "  - writes: models, pricing")?;
    }
    writeln!(out, "  - urls:")?;
    for url in sync.urls.as_deref().unwrap_or_default() {
        writeln!(out, "    - {url}")?;
    }
    Ok(())
}

fn sync_writes_models(sync: &AutoSync) -> bool {
    sync.writes
        .as_ref()
        .is_none_or(|writes| writes.contains(&AutoSyncWrite::Models))
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn agentic_diff_check(root: &Path) -> Result<()> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--numstat", "--"])
        .output()
        .context("running git diff --numstat for agentic registry sync")?;
    if !output.status.success() {
        bail!(
            "git diff --numstat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let issues = agentic_diff_issues_from_numstat(&stdout);
    if !issues.is_empty() {
        bail!("{}", issues.join("\n"));
    }
    println!("agentic registry diff check passed");
    Ok(())
}

fn agentic_diff_issues_from_numstat(numstat: &str) -> Vec<String> {
    let mut issues = Vec::new();
    for line in numstat.lines() {
        let mut parts = line.splitn(3, '\t');
        let _additions = parts.next();
        let _deletions = parts.next();
        let Some(path) = parts.next() else {
            continue;
        };
        if !path.starts_with("registry/providers/") && !path.starts_with("registry/models/") {
            issues.push(format!(
                "{path}: agentic sync may only edit files under registry/providers/ and registry/models/"
            ));
        }
    }
    issues
}

struct Artifacts {
    providers: String,
    models: String,
    provider_count: usize,
    model_count: usize,
}

fn build_artifacts(root: &Path) -> Result<Artifacts> {
    let loaded = load_registry(root)?;
    validate_loaded(&loaded)?;
    let mut providers: Vec<Value> = loaded
        .providers
        .iter()
        .map(provider_dist_value)
        .collect::<Result<Vec<_>>>()?;
    providers.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));

    let mut served_by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for provider in &loaded.providers {
        for model in resolved_models(&provider.data)? {
            let mut entry = model.clone();
            let id = entry
                .as_object_mut()
                .and_then(|obj| obj.remove("id"))
                .and_then(|v| v.as_str().map(ToOwned::to_owned))
                .context("resolved model missing id")?;
            entry
                .as_object_mut()
                .context("resolved model must be an object")?
                .insert(
                    "provider".to_string(),
                    Value::String(provider.data.name.clone()),
                );
            served_by.entry(id).or_default().push(entry);
        }
    }
    for providers_for_model in served_by.values_mut() {
        providers_for_model.sort_by(|a, b| a["provider"].as_str().cmp(&b["provider"].as_str()));
    }

    let mut canonical: Vec<CanonicalModel> = loaded.models().cloned().collect();
    canonical.sort_by(|a, b| a.id.cmp(&b.id));
    let mut models = Vec::with_capacity(canonical.len());
    for model in canonical {
        let mut value = serde_json::to_value(&model).context("serializing canonical model")?;
        value
            .as_object_mut()
            .context("canonical model must serialize as object")?
            .insert(
                "providers".to_string(),
                Value::Array(served_by.remove(&model.id).unwrap_or_default()),
            );
        models.push(value);
    }

    Ok(Artifacts {
        provider_count: providers.len(),
        model_count: models.len(),
        providers: serialize_data(providers)?,
        models: serialize_data(models)?,
    })
}

fn provider_dist_value(provider: &LoadedProvider) -> Result<Value> {
    let data = &provider.data;
    let mut value = serde_json::to_value(data).context("serializing provider")?;
    let obj = value
        .as_object_mut()
        .context("provider must serialize as object")?;
    let api_protocol = obj
        .remove("api_protocol")
        .unwrap_or(Value::Array(Vec::new()));
    let rate_limits = obj
        .remove("rate_limits")
        .unwrap_or(Value::Array(Vec::new()));
    obj.remove("models");
    obj.remove("protocol_endpoints");
    obj.remove("auto_sync");
    obj.insert("id".to_string(), Value::String(data.name.clone()));
    obj.insert(
        "required_config".to_string(),
        serde_json::to_value(resolved_required_config(data))
            .context("serializing required_config")?,
    );
    obj.insert(
        "byok".to_string(),
        Value::Bool(data.access == Access::ApiKey),
    );
    let protocol_endpoints = runtime_protocol_endpoints(data);
    if !protocol_endpoints.is_empty() {
        obj.insert(
            "protocol_endpoints".to_string(),
            serde_json::to_value(protocol_endpoints).context("serializing protocol_endpoints")?,
        );
    }
    if data.models.is_empty() {
        obj.insert("api_protocol".to_string(), api_protocol);
        obj.insert("rate_limits".to_string(), rate_limits);
        obj.insert("models".to_string(), Value::Array(Vec::new()));
    } else {
        obj.insert("models".to_string(), Value::Array(resolved_models(data)?));
    }
    Ok(value)
}

fn runtime_protocol_endpoints(provider: &ProviderFile) -> BTreeMap<&'static str, String> {
    provider
        .protocol_endpoints
        .iter()
        .map(|(protocol, endpoint)| (protocol.runtime_key(), endpoint.clone()))
        .collect()
}

fn resolved_models(provider: &ProviderFile) -> Result<Vec<Value>> {
    provider
        .models
        .iter()
        .map(|model| {
            let api_protocol = model
                .api_protocol
                .clone()
                .or_else(|| resolve_pattern(&provider.api_protocol, &model.id))
                .unwrap_or(ProtocolList::One(ApiProtocol::Openai));
            let rate_limits = model
                .rate_limits
                .clone()
                .or_else(|| resolve_pattern(&provider.rate_limits, &model.id));
            let mut obj = Map::new();
            obj.insert("id".to_string(), Value::String(model.id.clone()));
            obj.insert(
                "provider_model_id".to_string(),
                Value::String(model.provider_model_id.clone()),
            );
            obj.insert(
                "api_protocol".to_string(),
                serde_json::to_value(api_protocol).context("serializing api_protocol")?,
            );
            if let Some(pricing) = &model.pricing {
                obj.insert(
                    "pricing".to_string(),
                    serde_json::to_value(pricing).context("serializing pricing")?,
                );
            }
            if !model.capabilities.is_empty() {
                obj.insert(
                    "capabilities".to_string(),
                    serde_json::to_value(&model.capabilities)
                        .context("serializing capabilities")?,
                );
            }
            if let Some(rate_limits) = rate_limits {
                obj.insert(
                    "rate_limits".to_string(),
                    serde_json::to_value(rate_limits).context("serializing rate_limits")?,
                );
            }
            if let Some(deprecation_date) = &model.deprecation_date {
                obj.insert(
                    "deprecation_date".to_string(),
                    Value::String(deprecation_date.clone()),
                );
            }
            Ok(Value::Object(obj))
        })
        .collect()
}

fn serialize_data(data: Vec<Value>) -> Result<String> {
    let value = sort_value(json!({ "data": data }));
    let mut out = serde_json::to_string_pretty(&value).context("formatting dist JSON")?;
    out.push('\n');
    Ok(out)
}

fn sort_value(value: Value) -> Value {
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

#[derive(Debug)]
struct LoadedRegistry {
    model_files: Vec<LoadedModelFile>,
    providers: Vec<LoadedProvider>,
}

#[derive(Debug)]
struct LoadedModelFile {
    path: PathBuf,
    models: Vec<CanonicalModel>,
}

impl LoadedRegistry {
    fn models(&self) -> impl Iterator<Item = &CanonicalModel> + '_ {
        self.model_files.iter().flat_map(|file| file.models.iter())
    }
}

#[derive(Debug)]
struct LoadedProvider {
    path: PathBuf,
    data: ProviderFile,
}

fn load_registry(root: &Path) -> Result<LoadedRegistry> {
    let registry = root.join("registry");
    let model_files = load_canonical_models(&registry)?;
    let providers_dir = registry.join("providers");
    let mut providers = Vec::new();
    for entry in fs::read_dir(&providers_dir)
        .with_context(|| format!("reading {}", providers_dir.display()))?
    {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let data = read_yaml(&path)?;
        providers.push(LoadedProvider { path, data });
    }
    providers.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(LoadedRegistry {
        model_files,
        providers,
    })
}

fn load_canonical_models(registry: &Path) -> Result<Vec<LoadedModelFile>> {
    let models_dir = registry.join("models");
    let mut files = Vec::new();
    collect_yaml_files(&models_dir, &mut files)?;
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let models: Vec<CanonicalModel> = read_yaml(&path)?;
        out.push(LoadedModelFile { path, models });
    }
    Ok(out)
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
    let mut canonical_ids = HashSet::new();
    for model_file in &registry.model_files {
        let file = path_label(&model_file.path);
        let stem = model_file.path.file_stem().and_then(|s| s.to_str());
        for model in &model_file.models {
            if !valid_canonical_id(&model.id) {
                issues.push(format!(
                    "registry/models: '{}' is not a lowercase '<org>/<model>' id",
                    model.id
                ));
            }
            if !canonical_ids.insert(model.id.as_str()) {
                issues.push(format!(
                    "registry/models: duplicate canonical model '{}'",
                    model.id
                ));
            }
            if let Some((org, _)) = model.id.split_once('/')
                && Some(org) != stem
            {
                issues.push(format!(
                    "{file}: model '{}' belongs to vendor file '{org}.yaml'",
                    model.id
                ));
            }
            validate_canonical_model(model, &mut issues);
        }
    }

    let mut provider_names = HashMap::new();
    for provider in &registry.providers {
        validate_provider(
            provider,
            &canonical_ids,
            &mut provider_names,
            &mut issues,
            &mut advisories,
        );
    }

    if !issues.is_empty() {
        bail!("registry validation failed:\n  - {}", issues.join("\n  - "));
    }
    advisories.sort();
    Ok(advisories)
}

fn validate_canonical_model(model: &CanonicalModel, issues: &mut Vec<String>) {
    for modality in &model.input_modalities {
        if !matches!(modality.as_str(), "text" | "image" | "audio") {
            issues.push(format!(
                "registry/models: model '{}' has invalid input modality '{}'",
                model.id, modality
            ));
        }
    }
    for modality in &model.output_modalities {
        if !matches!(modality.as_str(), "text" | "audio") {
            issues.push(format!(
                "registry/models: model '{}' has invalid output modality '{}'",
                model.id, modality
            ));
        }
    }
    if let Some(date) = &model.release_date
        && !valid_yyyy_mm_dd(date)
    {
        issues.push(format!(
            "registry/models: model '{}' has invalid release_date '{}'",
            model.id, date
        ));
    }
    if let Some(date) = &model.knowledge_cutoff
        && !valid_yyyy_mm_or_dd(date)
    {
        issues.push(format!(
            "registry/models: model '{}' has invalid knowledge_cutoff '{}'",
            model.id, date
        ));
    }
}

fn validate_provider<'a>(
    provider: &'a LoadedProvider,
    canonical_ids: &HashSet<&str>,
    provider_names: &mut HashMap<&'a str, &'a Path>,
    issues: &mut Vec<String>,
    advisories: &mut Vec<String>,
) {
    let data = &provider.data;
    let file = path_label(&provider.path);
    if !valid_provider_name(&data.name) {
        issues.push(format!("{file}: invalid provider name '{}'", data.name));
    }
    let expected = format!("{}.yaml", data.name);
    if provider.path.file_name().and_then(|f| f.to_str()) != Some(expected.as_str()) {
        issues.push(format!(
            "{file}: filename does not match provider name '{}' (expected {expected})",
            data.name
        ));
    }
    if let Some(prior) = provider_names.insert(&data.name, &provider.path) {
        issues.push(format!(
            "{file}: provider name '{}' is also declared in {}",
            data.name,
            path_label(prior)
        ));
    }
    if let Some(api_base) = &data.api_base {
        validate_https(api_base, &file, "api_base", issues);
    }
    for (protocol, endpoint) in &data.protocol_endpoints {
        validate_https(
            endpoint,
            &file,
            &format!("protocol_endpoints.{}", protocol.source_key()),
            issues,
        );
    }
    validate_required_config(data, &file, issues);
    if let Some(url) = &data.doc_url {
        validate_https(url, &file, "doc_url", issues);
    }
    if let Some(metadata) = &data.metadata {
        validate_metadata(metadata, &file, issues);
    }
    validate_pattern_entries(&data.api_protocol, &file, "api_protocol", issues);
    validate_pattern_entries(&data.rate_limits, &file, "rate_limits", issues);
    validate_auth(data.auth.as_ref(), &file, issues);
    validate_auto_sync(data.auto_sync.as_ref(), &file, issues);

    if data.status == ProviderStatus::Active
        && data.models.is_empty()
        && data.auto_sync.is_none()
        && !matches!(data.access, Access::LocalOauth | Access::LocalPkce)
    {
        issues.push(format!(
            "{file}: provider '{}' is active but declares no models",
            data.name
        ));
    }

    let mut seen_models = HashSet::new();
    for model in &data.models {
        if !seen_models.insert(model.id.as_str()) {
            issues.push(format!(
                "{file}: provider '{}' declares model '{}' twice",
                data.name, model.id
            ));
        }
        // A provider may serve models beyond the curated `registry/models`
        // catalog (BYOK / BYO-subscription extras). The id must still be a
        // well-formed `<org>/<model>`; non-canonical ids are surfaced as a
        // non-failing advisory (typo-catch + curation backlog).
        if !valid_canonical_id(&model.id) {
            issues.push(format!(
                "{file}: model '{}' (provider_model_id={}) is not a valid lowercase '<org>/<model>' id",
                model.id, model.provider_model_id
            ));
        } else if !canonical_ids.contains(model.id.as_str()) {
            advisories.push(format!(
                "{file}: {} (provider_model_id={}) not in curated registry/models",
                model.id, model.provider_model_id
            ));
        }
        if let Some(protocols) = &model.api_protocol {
            validate_protocol_list(protocols, &file, "models.api_protocol", issues);
        }
        if let Some(pricing) = &model.pricing {
            validate_pricing(pricing, &file, &model.id, issues);
        }
        if let Some(date) = &model.deprecation_date
            && !valid_yyyy_mm_dd(date)
        {
            issues.push(format!(
                "{file}: model '{}' has invalid deprecation_date '{}'",
                model.id, date
            ));
        }
    }

    match data.billing {
        Billing::Subscription => {
            for model in &data.models {
                if model.pricing.is_some() {
                    issues.push(format!(
                        "{file}: subscription provider must not set per-token pricing (model '{}')",
                        model.id
                    ));
                }
            }
        }
        Billing::UsageToken => {
            for model in &data.models {
                if model.pricing.is_none() {
                    issues.push(format!(
                        "{file}: usage_token provider must set pricing for every model (model '{}')",
                        model.id
                    ));
                }
            }
        }
    }
}

fn resolved_required_config(provider: &ProviderFile) -> Vec<RequiredConfig> {
    if !provider.required_config.is_empty() {
        return provider.required_config.clone();
    }
    match provider.access {
        Access::ApiKey => vec![RequiredConfig::ApiKey],
        Access::LocalOauth => vec![RequiredConfig::LocalOauth],
        Access::LocalPkce => vec![RequiredConfig::LocalPkce],
        Access::Private => Vec::new(),
    }
}

fn validate_required_config(provider: &ProviderFile, file: &str, issues: &mut Vec<String>) {
    let required = resolved_required_config(provider);
    if provider.api_base.is_none() && !required.contains(&RequiredConfig::BaseUrl) {
        issues.push(format!(
            "{file}: providers without a fixed api_base must require base_url"
        ));
    }
    let mut seen = HashSet::new();
    for item in &required {
        if !seen.insert(*item) {
            issues.push(format!("{file}: required_config contains duplicate item"));
        }
    }
}

fn validate_pattern_entries<T>(
    entries: &[BTreeMap<String, T>],
    file: &str,
    field: &str,
    issues: &mut Vec<String>,
) {
    for (i, entry) in entries.iter().enumerate() {
        if entry.len() != 1 {
            issues.push(format!(
                "{file}: {field}[{i}] must contain exactly one pattern"
            ));
        }
    }
}

fn validate_metadata(metadata: &ProviderMetadata, file: &str, issues: &mut Vec<String>) {
    if metadata.headquarters.len() != 2
        || !metadata
            .headquarters
            .chars()
            .all(|c| c.is_ascii_uppercase())
    {
        issues.push(format!(
            "{file}: metadata.headquarters must be an ISO alpha-2 country code"
        ));
    }
    if !valid_slug(&metadata.slug, false) {
        issues.push(format!(
            "{file}: metadata.slug must be lowercase alphanumerics + hyphen"
        ));
    }
    for code in &metadata.datacenters {
        if !valid_region_code(code) {
            issues.push(format!(
                "{file}: metadata.datacenters entries must be uppercase region codes"
            ));
        }
    }
    for (field, value) in [
        (
            "metadata.privacy_policy_url",
            metadata.privacy_policy_url.as_ref(),
        ),
        (
            "metadata.status_page_url",
            metadata.status_page_url.as_ref(),
        ),
        (
            "metadata.terms_of_service_url",
            metadata.terms_of_service_url.as_ref(),
        ),
    ] {
        if let Some(url) = value {
            validate_https(url, file, field, issues);
        }
    }
}

fn validate_auth(auth: Option<&Auth>, file: &str, issues: &mut Vec<String>) {
    let Some(auth) = auth else {
        return;
    };
    match auth.kind {
        AuthKind::Bearer if auth.env.is_none() => {
            issues.push(format!("{file}: bearer auth requires env"));
        }
        AuthKind::Header if auth.env.is_none() || auth.header.is_none() => {
            issues.push(format!("{file}: header auth requires env and header"));
        }
        AuthKind::Oauth | AuthKind::Native if auth.handler.is_none() => {
            issues.push(format!("{file}: {:?} auth requires handler", auth.kind));
        }
        _ => {}
    }
}

fn validate_auto_sync(sync: Option<&AutoSync>, file: &str, issues: &mut Vec<String>) {
    let Some(sync) = sync else {
        return;
    };
    if sync.key.is_some() && sync.feed != AutoSyncFeed::ModelsDev {
        issues.push(format!(
            "{file}: auto_sync.key is only valid for models_dev"
        ));
    }
    if let Some(url) = &sync.url {
        if sync.feed != AutoSyncFeed::V1Models {
            issues.push(format!("{file}: auto_sync.url is only valid for v1_models"));
        }
        validate_https(url, file, "auto_sync.url", issues);
    }
    if let Some(urls) = &sync.urls {
        if sync.feed != AutoSyncFeed::Agentic {
            issues.push(format!("{file}: auto_sync.urls is only valid for agentic"));
        }
        if urls.is_empty() {
            issues.push(format!(
                "{file}: auto_sync.urls must contain at least one URL"
            ));
        }
        for url in urls {
            validate_https(url, file, "auto_sync.urls", issues);
        }
    }
    if sync.feed == AutoSyncFeed::Agentic && sync.urls.as_ref().is_none_or(Vec::is_empty) {
        issues.push(format!("{file}: auto_sync.urls is required for agentic"));
    }
}

fn validate_pricing(pricing: &ModelPricing, file: &str, model_id: &str, issues: &mut Vec<String>) {
    if pricing.context_tiers.is_empty() {
        return;
    }
    if pricing
        .input_tokens
        .as_ref()
        .and_then(|p| p.no_cache)
        .is_none()
        || pricing
            .output_tokens
            .as_ref()
            .and_then(|p| p.text)
            .is_none()
    {
        issues.push(format!(
            "{file}: model '{model_id}' context_tiers require base input_tokens.no_cache and output_tokens.text"
        ));
    }
    let mut prev = None;
    for tier in &pricing.context_tiers {
        if let Some(p) = prev
            && tier.above_input_tokens <= p
        {
            issues.push(format!(
                "{file}: model '{model_id}' context_tiers must strictly increase"
            ));
        }
        prev = Some(tier.above_input_tokens);
        if tier
            .input_tokens
            .as_ref()
            .and_then(|p| p.no_cache)
            .is_none()
            || tier.output_tokens.as_ref().and_then(|p| p.text).is_none()
        {
            issues.push(format!(
                "{file}: model '{model_id}' context tier must set no_cache and text rates"
            ));
        }
    }
}

fn validate_protocol_list(
    protocols: &ProtocolList,
    file: &str,
    field: &str,
    issues: &mut Vec<String>,
) {
    if matches!(protocols, ProtocolList::Many(v) if v.is_empty()) {
        issues.push(format!("{file}: {field} must not be an empty protocol set"));
    }
}

fn validate_https(url: &str, file: &str, field: &str, issues: &mut Vec<String>) {
    if !url.starts_with("https://") || reqwest::Url::parse(url).is_err() {
        issues.push(format!("{file}: {field} must be an HTTPS URL"));
    }
}

fn path_label(path: &Path) -> String {
    path.strip_prefix(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .unwrap_or(path)
        .display()
        .to_string()
}

fn valid_canonical_id(id: &str) -> bool {
    let Some((org, model)) = id.split_once('/') else {
        return false;
    };
    !model.contains('/') && valid_slug(org, true) && valid_slug(model, true)
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

fn valid_region_code(value: &str) -> bool {
    value.len() == 2 && value.chars().all(|c| c.is_ascii_uppercase())
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

fn valid_yyyy_mm_or_dd(value: &str) -> bool {
    (value.len() == 7 && value.as_bytes()[4] == b'-' || valid_yyyy_mm_dd(value))
        && value
            .chars()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn resolve_pattern<T: Clone>(entries: &[BTreeMap<String, T>], id: &str) -> Option<T> {
    let mut best: Option<(usize, &T)> = None;
    for entry in entries {
        let Some((pattern, value)) = entry.iter().next() else {
            continue;
        };
        if !pattern_matches(pattern, id) {
            continue;
        }
        let weight = pattern_specificity(pattern);
        if best.is_none_or(|(current, _)| weight > current) {
            best = Some((weight, value));
        }
    }
    best.map(|(_, value)| value.clone())
}

fn pattern_matches(pattern: &str, id: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return id.starts_with(prefix);
    }
    pattern == id
}

fn pattern_specificity(pattern: &str) -> usize {
    if pattern == "*" {
        0
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        prefix.len() + 1
    } else {
        pattern.len() + 2
    }
}

fn canonical_resolver<'a>(
    canonical_ids: impl IntoIterator<Item = &'a str>,
) -> impl Fn(&str) -> Option<String> {
    let mut by_full = HashMap::new();
    let mut by_slug: HashMap<String, Option<String>> = HashMap::new();
    for id in canonical_ids {
        by_full.insert(norm(id), id.to_string());
        let slug = norm(id.split_once('/').map(|(_, slug)| slug).unwrap_or(id));
        by_slug
            .entry(slug)
            .and_modify(|value| *value = None)
            .or_insert_with(|| Some(id.to_string()));
    }
    move |model_id| {
        if let Some(full) = by_full.get(&norm(model_id)) {
            return Some(full.clone());
        }
        if model_id.contains('/') {
            return None;
        }
        by_slug.get(&norm(model_id)).cloned().flatten()
    }
}

fn norm(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

async fn load_models_dev_catalog() -> Result<ModelsDevCatalog> {
    let body = reqwest::Client::builder()
        .user_agent(concat!("dist-helper/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building models.dev HTTP client")?
        .get("https://models.dev/api.json")
        .send()
        .await
        .context("fetching models.dev catalog")?
        .error_for_status()
        .context("models.dev returned an error")?
        .text()
        .await
        .context("reading models.dev response")?;
    serde_json::from_str(&body).context("parsing models.dev catalog")
}

fn pricing_from_cost(cost: Option<&ModelsDevCost>) -> Option<ModelPricing> {
    let cost = cost?;
    let input = InputTokenPricing {
        no_cache: clean_cost(cost.input),
        cache_read: clean_cost(cost.cache_read),
        cache_write: clean_cost(cost.cache_write),
    };
    let output = OutputTokenPricing {
        text: clean_cost(cost.output),
        reasoning: None,
    };
    if input.no_cache.is_none()
        && input.cache_read.is_none()
        && input.cache_write.is_none()
        && output.text.is_none()
    {
        return None;
    }
    Some(ModelPricing {
        input_tokens: Some(input),
        output_tokens: Some(output),
        context_tiers: Vec::new(),
    })
}

fn clean_cost(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && *v >= 0.0)
}

fn append_models_to_provider(path: &Path, adds: &[ProviderModel]) -> Result<()> {
    let mut raw =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.contains("models: []") {
        raw = raw.replacen("models: []", "models:", 1);
    }
    if !raw.ends_with('\n') {
        raw.push('\n');
    }
    let insert_at = models_insert_offset(&raw)
        .with_context(|| format!("locating models list in {}", path.display()))?;
    let mut append = String::new();
    for model in adds {
        append.push_str(&render_model_append(model));
    }
    raw.insert_str(insert_at, &append);
    let parsed: ProviderFile = serde_saphyr::from_str(&raw)
        .with_context(|| format!("validating updated {}", path.display()))?;
    if parsed.name.is_empty() {
        bail!("updated provider file has empty name: {}", path.display());
    }
    fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn models_insert_offset(raw: &str) -> Result<usize> {
    let mut offset = 0;
    let mut in_models = false;
    let mut insert_at = None;

    for line in raw.split_inclusive('\n') {
        let trimmed_eol = line.trim_end_matches(['\r', '\n']);
        if in_models {
            let is_top_level = !trimmed_eol.is_empty()
                && !trimmed_eol.starts_with([' ', '\t'])
                && (trimmed_eol.contains(':') || trimmed_eol.starts_with('#'));
            if is_top_level {
                insert_at = Some(offset);
                break;
            }
        } else if trimmed_eol == "models:" {
            in_models = true;
        }
        offset += line.len();
    }

    if !in_models {
        bail!("provider file does not contain a models list");
    }
    Ok(insert_at.unwrap_or(raw.len()))
}

fn render_model_append(model: &ProviderModel) -> String {
    let mut out = format!(
        "  - id: {}\n    provider_model_id: {}\n",
        model.id, model.provider_model_id
    );
    if let Some(pricing) = &model.pricing {
        out.push_str("    pricing:\n");
        if let Some(input) = &pricing.input_tokens
            && (input.no_cache.is_some()
                || input.cache_read.is_some()
                || input.cache_write.is_some())
        {
            out.push_str("      input_tokens:\n");
            if let Some(v) = input.no_cache {
                out.push_str(&format!("        no_cache: {v}\n"));
            }
            if let Some(v) = input.cache_read {
                out.push_str(&format!("        cache_read: {v}\n"));
            }
            if let Some(v) = input.cache_write {
                out.push_str(&format!("        cache_write: {v}\n"));
            }
        }
        if let Some(output) = &pricing.output_tokens
            && let Some(v) = output.text
        {
            out.push_str("      output_tokens:\n");
            out.push_str(&format!("        text: {v}\n"));
        }
    }
    out
}

fn dist_dir(root: &Path) -> PathBuf {
    root.join("dist").join("registry")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CanonicalModel {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    output_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    knowledge_cutoff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open_weights: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    family: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderFile {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<ProviderMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    api_protocol: Vec<BTreeMap<String, ProtocolList>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rate_limits: Vec<BTreeMap<String, RateLimits>>,
    models: Vec<ProviderModel>,
    status: ProviderStatus,
    #[serde(default = "default_weight")]
    weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submitted_at: Option<String>,
    #[serde(default)]
    community: bool,
    #[serde(default)]
    access: Access,
    #[serde(default)]
    billing: Billing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_base: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    protocol_endpoints: BTreeMap<ApiProtocol, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_config: Vec<RequiredConfig>,
    #[serde(default)]
    auth_scheme: AuthScheme,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<Auth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    doc_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_sync: Option<AutoSync>,
}

fn default_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderMetadata {
    headquarters: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    datacenters: Vec<String>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    privacy_policy_url: Option<String>,
    slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status_page_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terms_of_service_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderModel {
    id: String,
    provider_model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_protocol: Option<ProtocolList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pricing: Option<ModelPricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limits: Option<RateLimits>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deprecation_date: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ApiProtocol {
    Openai,
    Anthropic,
    Google,
    Responses,
    /// Google Antigravity Code Assist — a custom, externally-registered runtime
    /// protocol (`bitrouter_providers::antigravity`). No models.dev source.
    Antigravity,
}

impl ApiProtocol {
    fn source_key(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Responses => "responses",
            Self::Antigravity => "antigravity",
        }
    }

    fn runtime_key(self) -> &'static str {
        match self {
            Self::Openai => "chat_completions",
            Self::Anthropic => "messages",
            Self::Google => "generate_content",
            Self::Responses => "responses",
            // The runtime maps any unknown protocol string to `Custom(_)`; this
            // is the name the antigravity adapter registers under.
            Self::Antigravity => "antigravity",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
enum ProtocolList {
    One(ApiProtocol),
    Many(Vec<ApiProtocol>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Capability {
    StructuredOutputs,
    Tools,
    Reasoning,
    WebSearch,
    Logprobs,
    ImageInput,
    AudioInput,
    VideoInput,
    FileInput,
    ImageOutput,
    AudioOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ProviderStatus {
    Active,
    Staging,
    Suspended,
    Withdrawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Access {
    #[default]
    ApiKey,
    LocalOauth,
    LocalPkce,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RequiredConfig {
    ApiKey,
    BaseUrl,
    LocalOauth,
    LocalPkce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Billing {
    #[default]
    #[serde(alias = "token")]
    UsageToken,
    Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum AuthScheme {
    #[default]
    XApiKey,
    Bearer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProviderKind {
    FirstParty,
    Gateway,
    Cloud,
    ThirdParty,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    kind: AuthKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra_headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handler: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthKind {
    Bearer,
    Header,
    Oauth,
    Native,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AutoSync {
    feed: AutoSyncFeed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    urls: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    writes: Option<Vec<AutoSyncWrite>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AutoSyncFeed {
    ModelsDev,
    V1Models,
    Agentic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AutoSyncWrite {
    Models,
    Pricing,
}

impl AutoSyncWrite {
    fn source_key(self) -> &'static str {
        match self {
            Self::Models => "models",
            Self::Pricing => "pricing",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RateLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requests_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens_per_minute: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelPricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<InputTokenPricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<OutputTokenPricing>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context_tiers: Vec<ContextTier>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InputTokenPricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    no_cache: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OutputTokenPricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextTier {
    above_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<InputTokenPricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<OutputTokenPricing>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCatalog {
    #[serde(flatten)]
    providers: BTreeMap<String, ModelsDevProvider>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: BTreeMap<String, ModelsDevModel>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    cost: Option<ModelsDevCost>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn canonical_resolver_matches_full_ids_and_unique_bare_slugs() {
        let resolve =
            canonical_resolver(["anthropic/claude-sonnet-4.6", "openai/gpt-5.5", "x/gpt-5.5"]);
        assert_eq!(
            resolve("claude-sonnet-4-6").as_deref(),
            Some("anthropic/claude-sonnet-4.6")
        );
        assert_eq!(
            resolve("anthropic/claude-sonnet-4.6").as_deref(),
            Some("anthropic/claude-sonnet-4.6")
        );
        assert_eq!(resolve("gpt-5.5"), None, "ambiguous bare slug");
        assert_eq!(resolve("other/claude-sonnet-4-6"), None);
    }

    #[test]
    fn resolved_pattern_uses_longest_match() {
        let entries = vec![
            BTreeMap::from([("*".to_string(), 1)]),
            BTreeMap::from([("anthropic/*".to_string(), 2)]),
            BTreeMap::from([("anthropic/claude-sonnet-4.6".to_string(), 3)]),
        ];
        assert_eq!(
            resolve_pattern(&entries, "anthropic/claude-sonnet-4.6"),
            Some(3)
        );
        assert_eq!(
            resolve_pattern(&entries, "anthropic/claude-haiku-4.5"),
            Some(2)
        );
        assert_eq!(resolve_pattern(&entries, "openai/gpt-5.5"), Some(1));
    }

    #[test]
    fn serialize_data_sorts_object_keys_recursively() {
        let json = serialize_data(vec![json!({"z": 1, "a": {"b": 2, "a": 1}})]).unwrap();
        assert!(json.ends_with("}\n"));
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let keys: Vec<_> = parsed["data"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, vec!["a", "z"]);
        let nested: Vec<_> = parsed["data"][0]["a"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(nested, vec!["a", "b"]);
    }

    #[test]
    fn load_canonical_models_reads_sequence_per_file() {
        let root = test_root("model-sequence");
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/one
    provider_model_id: one
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
  - id: acme/two
    provider_model_id: two
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
"#,
        );
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/one
  name: "Acme: One"
  input_modalities: [text]
  output_modalities: [text]
- id: acme/two
  name: "Acme: Two"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );

        let loaded = load_registry(&root).expect("loads grouped model file");
        let ids: Vec<_> = loaded.models().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["acme/one", "acme/two"]);
        validate_loaded(&loaded).expect("grouped model file validates");
    }

    #[test]
    fn load_registry_reads_recursive_model_files() {
        let root = test_root("models-dir");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/test-model
  name: "Acme: Test Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/test-model
    provider_model_id: test-model
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
"#,
        );

        let loaded = load_registry(&root).expect("loads registry/models/**/*.yaml");

        assert_eq!(loaded.models().count(), 1);
        assert_eq!(
            loaded.models().next().map(|m| m.id.as_str()),
            Some("acme/test-model")
        );
        validate_loaded(&loaded).expect("model file registry validates");
    }

    #[test]
    fn model_id_org_must_match_filename() {
        let root = test_root("org-stem-mismatch");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: other/model
  name: "Other: Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );

        let loaded = load_registry(&root).expect("loads");
        let err = validate_loaded(&loaded).expect_err("org/stem mismatch must fail validation");
        assert!(
            err.to_string()
                .contains("belongs to vendor file 'other.yaml'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_artifacts_emits_required_config_and_omits_unset_api_base() {
        let root = test_root("required-config");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/test-model
  name: "Acme: Test Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  datacenters: [US, EU]
  name: Acme
  slug: acme
  privacy_policy_url: https://acme.test/privacy
  terms_of_service_url: https://acme.test/terms
api_protocol:
  - "*": openai
protocol_endpoints:
  anthropic: https://api.acme.test/anthropic
required_config:
  - api_key
  - base_url
models:
  - id: acme/test-model
    provider_model_id: test-model
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
"#,
        );

        let artifacts = build_artifacts(&root).expect("builds provider without fixed api_base");
        let providers: Value = serde_json::from_str(&artifacts.providers).unwrap();
        let provider = &providers["data"][0];

        assert_eq!(provider["required_config"], json!(["api_key", "base_url"]));
        assert_eq!(provider["metadata"]["datacenters"], json!(["US", "EU"]));
        assert_eq!(
            provider["protocol_endpoints"],
            json!({ "messages": "https://api.acme.test/anthropic" })
        );
        assert!(provider.get("api_base").is_none());
    }

    #[test]
    fn build_artifacts_strips_source_catalog_hints_from_public_dist() {
        let root = test_root("strip-auto-sync");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/test-model
  name: "Acme: Test Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/test-model
    provider_model_id: test-model
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
auto_sync:
  feed: models_dev
"#,
        );

        let artifacts = build_artifacts(&root).expect("builds registry dist");
        let providers: Value = serde_json::from_str(&artifacts.providers).unwrap();
        let provider = &providers["data"][0];

        assert!(
            provider.get("auto_sync").is_none(),
            "public dist must not expose maintainer-only catalog sync hints"
        );
    }

    #[test]
    fn append_models_to_provider_inserts_inside_models_list() {
        let root = test_root("append-model");
        let provider_path = root.join("registry/providers/acme.yaml");
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
api_protocol:
  - "*": openai
models:
  - id: acme/one
    provider_model_id: one
status: active
api_base: https://api.acme.test/v1
auto_sync:
  feed: models_dev
"#,
        );
        let add = ProviderModel {
            id: "acme/two".to_string(),
            provider_model_id: "two".to_string(),
            api_protocol: None,
            pricing: None,
            rate_limits: None,
            capabilities: Vec::new(),
            deprecation_date: None,
        };

        append_models_to_provider(&provider_path, &[add]).expect("append keeps YAML valid");

        let raw = fs::read_to_string(&provider_path).unwrap();
        assert!(
            raw.contains("  - id: acme/two\n    provider_model_id: two\nstatus: active"),
            "new model is appended before the next top-level key: {raw}"
        );
        let parsed: ProviderFile = serde_saphyr::from_str(&raw).unwrap();
        assert_eq!(parsed.models.len(), 2);
    }

    #[test]
    fn v1_models_catalog_attaches_known_canonical_models_only() {
        let provider: ProviderFile = serde_saphyr::from_str(
            r#"
name: acme
api_protocol:
  - "*": openai
models:
  - id: openai/gpt-5.5
    provider_model_id: gpt-5.5
status: active
billing: subscription
api_base: https://api.acme.test/v1
auto_sync:
  feed: v1_models
"#,
        )
        .unwrap();
        let resolve = canonical_resolver(["openai/gpt-5.5", "anthropic/claude-sonnet-4.6"]);
        let body = r#"
{
  "object": "list",
  "data": [
    { "id": "gpt-5.5", "object": "model" },
    { "id": "claude-sonnet-4-6", "object": "model" },
    { "id": "not-yet-canonical", "object": "model" }
  ]
}
"#;

        let plan = v1_models_plan_for_provider(&provider, body, &resolve).unwrap();

        assert_eq!(plan.unresolved, vec!["not-yet-canonical"]);
        assert_eq!(plan.adds.len(), 1);
        assert_eq!(plan.adds[0].id, "anthropic/claude-sonnet-4.6");
        assert_eq!(plan.adds[0].provider_model_id, "claude-sonnet-4-6");
        assert!(plan.adds[0].pricing.is_none());
    }

    #[test]
    fn v1_models_catalog_copies_pricing_when_present() {
        let provider: ProviderFile = serde_saphyr::from_str(
            r#"
name: acme
api_protocol:
  - "*": openai
models: []
status: active
api_base: https://api.acme.test/v1
auto_sync:
  feed: v1_models
"#,
        )
        .unwrap();
        let resolve = canonical_resolver(["openai/gpt-5.5"]);
        let body = r#"
{
  "data": [
    {
      "id": "openai/gpt-5.5",
      "pricing": {
        "input_tokens": {
          "no_cache": 1.5,
          "cache_read": 0.15
        },
        "output_tokens": {
          "text": 6.0
        }
      }
    }
  ]
}
"#;

        let plan = v1_models_plan_for_provider(&provider, body, &resolve).unwrap();

        assert_eq!(plan.adds.len(), 1);
        let pricing = plan.adds[0].pricing.as_ref().expect("pricing copied");
        let input = pricing.input_tokens.as_ref().expect("input pricing");
        let output = pricing.output_tokens.as_ref().expect("output pricing");
        assert_eq!(input.no_cache, Some(1.5));
        assert_eq!(input.cache_read, Some(0.15));
        assert_eq!(output.text, Some(6.0));
    }

    #[test]
    fn v1_models_catalog_skips_usage_token_models_without_pricing() {
        let provider: ProviderFile = serde_saphyr::from_str(
            r#"
name: acme
api_protocol:
  - "*": openai
models: []
status: active
api_base: https://api.acme.test/v1
auto_sync:
  feed: v1_models
"#,
        )
        .unwrap();
        let resolve = canonical_resolver(["openai/gpt-5.5"]);
        let body = r#"
{
  "data": [
    { "id": "openai/gpt-5.5" }
  ]
}
"#;

        let plan = v1_models_plan_for_provider(&provider, body, &resolve).unwrap();

        assert!(plan.adds.is_empty());
    }

    #[test]
    fn v1_models_sync_uses_public_endpoint_even_with_runtime_auth() {
        let provider: ProviderFile = serde_saphyr::from_str(
            r#"
name: acme
api_protocol:
  - "*": openai
models: []
status: active
api_base: https://api.acme.test/v1
auth:
  kind: bearer
  env: ACME_API_KEY
auto_sync:
  feed: v1_models
"#,
        )
        .unwrap();

        assert_eq!(v1_auth_headers(&provider), Vec::<(String, String)>::new());
    }

    #[test]
    fn agentic_sync_requires_urls_and_renders_task_prompt() {
        let root = test_root("agentic-prompt");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/test-model
  name: "Acme: Test Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/test-model
    provider_model_id: test-model
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
auto_sync:
  feed: agentic
  urls:
    - https://docs.acme.test/models
    - https://docs.acme.test/pricing
"#,
        );

        validate(&root).expect("agentic sync with URLs validates");
        let prompt = agentic_prompt(&root).expect("renders agentic sync prompt");

        assert!(prompt.contains("- `acme` (`registry/providers/acme.yaml`)"));
        assert!(prompt.contains("https://docs.acme.test/models"));
        assert!(prompt.contains("cargo run -p dist-helper -- registry validate"));
        assert!(prompt.contains("If the listed URLs are unreachable"));
        assert!(prompt.contains("Do not remove or edit provider `auto_sync`"));
        assert!(prompt.contains("current source count, not a limit"));
        assert!(prompt.contains("existing_model_count: 1"));
        assert!(prompt.contains("writes: models, pricing"));
        assert!(prompt.contains("Raw HTML or rendered app HTML is still readable source material"));
        assert!(prompt.contains("Do not use truncated output"));
        assert!(prompt.contains("mkdir -p target/agentic-sync"));
        assert!(prompt.contains("curl -sS -L"));
        assert!(prompt.contains("rg"));
        assert!(prompt.contains("Do not fetch `_next/`, static assets, JavaScript chunks"));
        assert!(prompt.contains("Do not print large raw HTML, YAML, or JSON files"));
        assert!(prompt.contains("generic extraction strategies"));
        assert!(prompt.contains("Do not use YAML serializers"));
        assert!(prompt.contains("Do not omit confirmed public models just to keep the diff small"));
        assert!(
            prompt.contains("A newly added provider model entry may include its own `pricing`")
        );
        assert!(prompt.contains("re-check pricing for every provider model"));
        assert!(prompt.contains("leave pricing unchanged only when it cannot be confirmed"));
        assert!(prompt.contains("preserve `pricing` in all pre-existing model entries exactly"));
        assert!(prompt.contains("Registry pricing values are USD per 1 million tokens"));
        assert!(prompt.contains("Credits, points, coins, or other provider-internal units"));
        assert!(prompt.contains("do not copy provider-internal unit numbers into pricing"));
        assert!(prompt.contains("include the exact `registry valid:` output line"));
        assert!(!prompt.contains("canonical_models_json"));
        assert!(!prompt.contains("; model_count: 1"));
    }

    #[test]
    fn agentic_diff_check_allows_large_provider_rewrites() {
        let issues = agentic_diff_issues_from_numstat(
            "12\t179\tregistry/providers/worldrouter.yaml\n\
             70\t0\tregistry/providers/another.yaml\n\
             12\t179\tregistry/models/anthropic/example.yaml\n",
        );

        assert!(issues.is_empty());
    }

    #[test]
    fn agentic_diff_check_rejects_non_registry_source_paths() {
        let issues = agentic_diff_issues_from_numstat(
            "12\t0\tregistry/providers/worldrouter.yaml\n\
             1\t0\thelpers/dist-helper/src/registry.rs\n",
        );

        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("helpers/dist-helper/src/registry.rs"));
        assert!(issues[0].contains("registry/providers/"));
        assert!(issues[0].contains("registry/models/"));
    }

    #[test]
    fn registry_sync_workflow_uses_agentic_defaults() {
        let workflow = include_str!("../../../.github/workflows/registry-sync.yml");

        assert!(workflow.contains(r#"cron: "0 22 * * *""#));
        assert!(workflow.contains("AGENTIC_SYNC_MODEL: moonshotai/kimi-k2.7-code"));
        assert!(workflow.contains("uses: actions/create-github-app-token@v2"));
        assert!(workflow.contains("app-id: ${{ secrets.APP_ID }}"));
        assert!(workflow.contains("private-key: ${{ secrets.APP_PRIVATE_KEY }}"));
        assert!(workflow.contains("token: ${{ steps.generate-token.outputs.token }}"));
        assert!(workflow.contains("GH_TOKEN: ${{ steps.generate-token.outputs.token }}"));
        assert!(workflow.contains(r#"git config user.name "bitrouter-automation[bot]""#));
        assert!(workflow.contains(
            r#"git config user.email "267229870+bitrouter-automation[bot]@users.noreply.github.com""#
        ));
    }

    #[test]
    fn tencent_tokenhub_base_urls_match_official_hosts() {
        let root = crate::workspace_root();
        let loaded = load_registry(&root).expect("loads checked-in registry");
        let api_base = |name: &str| {
            loaded
                .providers
                .iter()
                .find(|provider| provider.data.name == name)
                .unwrap_or_else(|| panic!("missing provider {name}"))
                .data
                .api_base
                .as_deref()
                .unwrap_or_else(|| panic!("provider {name} must set api_base"))
                .to_string()
        };

        assert_eq!(
            api_base("tencent"),
            "https://tokenhub-intl.tencentcloudmaas.com/v1"
        );
        assert_eq!(
            api_base("tencent_cn"),
            "https://tokenhub.tencentmaas.com/v1"
        );
    }

    #[test]
    fn agentic_sync_without_urls_is_invalid() {
        let root = test_root("agentic-missing-urls");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/test-model
  name: "Acme: Test Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/test-model
    provider_model_id: test-model
status: active
api_base: https://api.acme.test/v1
auto_sync:
  feed: agentic
"#,
        );

        let err = validate(&root).expect_err("agentic sync requires URLs");
        assert!(format!("{err:#}").contains("auto_sync.urls is required for agentic"));
    }

    #[test]
    fn subscription_provider_must_not_price() {
        let root = test_root("sub-priced");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/one
  name: "Acme: One"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
billing: subscription
models:
  - id: acme/one
    provider_model_id: one
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
"#,
        );
        let loaded = load_registry(&root).expect("loads");
        let err = validate_loaded(&loaded).expect_err("subscription+pricing must fail");
        assert!(
            err.to_string().contains("subscription"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn usage_provider_must_price_every_model() {
        let root = test_root("usage-unpriced");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/one
  name: "Acme: One"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/one
    provider_model_id: one
status: active
api_base: https://api.acme.test/v1
"#,
        );
        let loaded = load_registry(&root).expect("loads");
        let err = validate_loaded(&loaded).expect_err("usage without pricing must fail");
        assert!(err.to_string().contains("usage"), "unexpected error: {err}");
    }

    #[test]
    fn provider_may_list_non_canonical_model_as_advisory() {
        let root = test_root("byok-advisory");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/one
  name: "Acme: One"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        // `acme/byok-extra` is well-formed but NOT in the curated catalog.
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/one
    provider_model_id: one
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
  - id: acme/byok-extra
    provider_model_id: byok-extra
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
"#,
        );
        let loaded = load_registry(&root).expect("loads");
        let advisories = validate_loaded(&loaded).expect("non-canonical provider model is allowed");
        assert!(
            advisories.iter().any(|a| a.contains("acme/byok-extra")),
            "expected an advisory for acme/byok-extra, got: {advisories:?}"
        );
    }

    #[test]
    fn provider_malformed_model_id_is_invalid() {
        let root = test_root("byok-malformed");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/one
  name: "Acme: One"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );
        // Uppercase org is not a valid lowercase `<org>/<model>` id.
        write(
            &root,
            "registry/providers/acme.yaml",
            r#"
name: acme
metadata:
  headquarters: US
  name: Acme
  slug: acme
api_protocol:
  - "*": openai
models:
  - id: acme/one
    provider_model_id: one
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
  - id: Acme/Bad-Id
    provider_model_id: bad
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
status: active
api_base: https://api.acme.test/v1
"#,
        );
        let loaded = load_registry(&root).expect("loads");
        let err = validate_loaded(&loaded).expect_err("malformed provider model id must fail");
        assert!(
            err.to_string().contains("valid lowercase"),
            "unexpected error: {err}"
        );
    }

    fn test_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bitrouter-dist-helper-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("registry/providers")).unwrap();
        root
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents.trim_start()).unwrap();
    }
}
