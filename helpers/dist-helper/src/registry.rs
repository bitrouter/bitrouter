use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use bitrouter_sdk::language_model::types::ReasoningEffortConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub fn validate(root: &Path) -> Result<()> {
    let loaded = load_registry(root)?;
    let advisories = validate_loaded(&loaded)?;
    println!(
        "registry valid: {} canonical models, {} providers, {} agents, {} runtimes",
        loaded.models().count(),
        loaded.providers.len(),
        loaded.agents().count(),
        loaded.runtimes.len()
    );
    if !advisories.is_empty() {
        println!(
            "note: {} advisory(ies) — non-curated provider models (BYOK / \
             BYO-subscription extras) and unpinned agent invocations:",
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
    let documents = [
        ("providers.json", &artifacts.providers),
        ("models.json", &artifacts.models),
        ("agents.json", &artifacts.agents),
        ("runtimes.json", &artifacts.runtimes),
    ];
    if check {
        for (name, rendered) in documents {
            let path = dist_dir(root).join(name);
            let current =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            if &current != rendered {
                bail!(
                    "registry dist is stale ({name}) - run `cargo run -p dist-helper -- registry build` and commit dist/registry"
                );
            }
        }
        println!(
            "registry dist is up to date: {} providers, {} canonical models, {} runtimes, {} agents",
            artifacts.provider_count,
            artifacts.model_count,
            artifacts.runtime_count,
            artifacts.agent_count
        );
        return Ok(());
    }
    fs::create_dir_all(dist_dir(root))
        .with_context(|| format!("creating {}", dist_dir(root).display()))?;
    for (name, rendered) in documents {
        let path = dist_dir(root).join(name);
        fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
    }
    println!(
        "wrote dist/registry: {} providers, {} canonical models, {} runtimes, {} agents",
        artifacts.provider_count,
        artifacts.model_count,
        artifacts.runtime_count,
        artifacts.agent_count
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
        let adds = models_dev_plan_for_provider(&provider.data, models, &resolve);
        if !adds.is_empty() {
            attaches
                .entry(provider.data.name.clone())
                .or_default()
                .extend(adds);
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

fn models_dev_plan_for_provider(
    provider: &ProviderFile,
    catalog: &ModelsDevProvider,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Vec<ProviderModel> {
    let have: HashSet<&str> = provider
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    let have_provider_model_ids: HashSet<&str> = provider
        .models
        .iter()
        .map(|model| model.provider_model_id.as_str())
        .collect();
    let mut staged = HashSet::new();
    let subscription = provider.billing == Billing::Subscription;
    let mut adds = Vec::new();

    for (model_id, model) in &catalog.models {
        if have_provider_model_ids.contains(model_id.as_str()) {
            continue;
        }
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
        adds.push(ProviderModel {
            id: canonical_id,
            provider_model_id: model_id.clone(),
            api_protocol: None,
            pricing,
            rate_limits: None,
            compatibility: None,
            capabilities: Vec::new(),
            reasoning_effort: None,
            deprecation_date: None,
        });
    }

    adds
}

fn v1_models_plan_for_provider(
    provider: &ProviderFile,
    body: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Result<V1ModelsPlan> {
    let catalog: V1ModelsResponse =
        serde_json::from_str(body).context("parsing OpenAI-compatible /models response")?;
    let have: HashSet<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
    let have_provider_model_ids: HashSet<&str> = provider
        .models
        .iter()
        .map(|model| model.provider_model_id.as_str())
        .collect();
    let mut staged = HashSet::new();
    let mut unresolved_seen = HashSet::new();
    let mut adds = Vec::new();
    let mut unresolved = Vec::new();

    for model in catalog.data {
        if have_provider_model_ids.contains(model.id.as_str()) {
            continue;
        }
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
            compatibility: None,
            capabilities: Vec::new(),
            reasoning_effort: None,
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
    agents: String,
    runtimes: String,
    provider_count: usize,
    model_count: usize,
    agent_count: usize,
    runtime_count: usize,
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

    Ok(Artifacts {
        provider_count: providers.len(),
        model_count: models.len(),
        agent_count: agents.len(),
        runtime_count: runtimes.len(),
        providers: serialize_data(providers)?,
        models: serialize_data(models)?,
        agents: serialize_data(agents)?,
        runtimes: serialize_data(runtimes)?,
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
            if let Some(reasoning_effort) = &model.reasoning_effort {
                obj.insert(
                    "reasoning_effort".to_string(),
                    serde_json::to_value(reasoning_effort)
                        .context("serializing reasoning_effort")?,
                );
            }
            if let Some(rate_limits) = rate_limits {
                obj.insert(
                    "rate_limits".to_string(),
                    serde_json::to_value(rate_limits).context("serializing rate_limits")?,
                );
            }
            if let Some(compatibility) = &model.compatibility {
                obj.insert(
                    "compatibility".to_string(),
                    serde_json::to_value(compatibility)
                        .context("serializing model compatibility")?,
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

#[derive(Debug)]
struct LoadedRegistry {
    model_files: Vec<LoadedModelFile>,
    providers: Vec<LoadedProvider>,
    agent_files: Vec<LoadedAgentFile>,
    runtimes: Vec<LoadedRuntime>,
}

#[derive(Debug)]
struct LoadedModelFile {
    path: PathBuf,
    models: Vec<CanonicalModel>,
}

#[derive(Debug)]
struct LoadedAgentFile {
    path: PathBuf,
    agents: Vec<CanonicalAgent>,
}

impl LoadedRegistry {
    fn models(&self) -> impl Iterator<Item = &CanonicalModel> + '_ {
        self.model_files.iter().flat_map(|file| file.models.iter())
    }

    fn agents(&self) -> impl Iterator<Item = &CanonicalAgent> + '_ {
        self.agent_files.iter().flat_map(|file| file.agents.iter())
    }
}

#[derive(Debug)]
struct LoadedProvider {
    path: PathBuf,
    data: ProviderFile,
}

#[derive(Debug)]
struct LoadedRuntime {
    path: PathBuf,
    data: RuntimeFile,
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

    // `agents/` and `runtimes/` are additive primitives: a registry tree
    // without them loads as one with no agents, rather than failing. Keeps
    // minimal fixture trees (and any older mirror) valid.
    let mut agent_files = Vec::new();
    for path in optional_yaml_files(&registry.join("agents"))? {
        let agents: Vec<CanonicalAgent> = read_yaml(&path)?;
        agent_files.push(LoadedAgentFile { path, agents });
    }

    let mut runtimes = Vec::new();
    for path in optional_yaml_files(&registry.join("runtimes"))? {
        let data = read_yaml(&path)?;
        runtimes.push(LoadedRuntime { path, data });
    }

    Ok(LoadedRegistry {
        model_files,
        providers,
        agent_files,
        runtimes,
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
            reject_reserved_namespace(&model.id, "registry/models", &mut issues);
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

/// BitRouter's own model namespace — the `bitrouter/` in `bitrouter/auto`.
///
/// Stage-0 resolution claims this whole prefix and resolves it locally, so a
/// catalog entry underneath it could never be reached by a request: the router
/// answers first. Keeping the namespace empty here is the other half of that
/// bargain, and it keeps the reserved slugs meaning one thing whether a caller
/// is talking to a local daemon or to BitRouter Cloud.
const RESERVED_MODEL_NAMESPACE: &str = "bitrouter/";

/// Reject a model id that lands in the reserved namespace. Applied to both the
/// curated catalog and provider-declared models, because a provider may serve
/// ids beyond the catalog and those are otherwise only advisory.
fn reject_reserved_namespace(model_id: &str, context: &str, issues: &mut Vec<String>) {
    if model_id.starts_with(RESERVED_MODEL_NAMESPACE) {
        issues.push(format!(
            "{context}: model '{model_id}' uses the reserved '{RESERVED_MODEL_NAMESPACE}' \
             namespace, which BitRouter resolves locally and no provider may declare"
        ));
    }
}

fn validate_canonical_model(model: &CanonicalModel, issues: &mut Vec<String>) {
    for modality in &model.input_modalities {
        if !matches!(modality.as_str(), "text" | "image" | "audio" | "video") {
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
    if let Some(benchmarks) = &model.benchmarks {
        validate_terminal_bench_2_1(&model.id, benchmarks.terminal_bench_2_1.as_ref(), issues);
    }
}

/// Sanity-check any recorded Terminal-Bench 2.1 metrics. Every metric is
/// optional (they stay absent until measured), so this only fires on values
/// that are present and out of range — accuracy is a `0..=100` percent,
/// cost/time are non-negative, and `as_of` is a calendar date.
fn validate_terminal_bench_2_1(
    model_id: &str,
    tb: Option<&TerminalBench21>,
    issues: &mut Vec<String>,
) {
    let Some(tb) = tb else { return };
    if let Some(accuracy) = tb.accuracy
        && !(0.0..=100.0).contains(&accuracy)
    {
        issues.push(format!(
            "registry/models: model '{model_id}' terminal_bench_2_1.accuracy {accuracy} is outside 0..=100"
        ));
    }
    for (field, value) in [
        ("cost_per_task", tb.cost_per_task),
        ("time_per_task", tb.time_per_task),
    ] {
        if let Some(value) = value
            && value < 0.0
        {
            issues.push(format!(
                "registry/models: model '{model_id}' terminal_bench_2_1.{field} {value} is negative"
            ));
        }
    }
    if let Some(date) = &tb.as_of
        && !valid_yyyy_mm_dd(date)
    {
        issues.push(format!(
            "registry/models: model '{model_id}' terminal_bench_2_1.as_of '{date}' is not YYYY-MM-DD"
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

    if data.status == EntryStatus::Active
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
    let mut seen_provider_model_ids = HashSet::new();
    for model in &data.models {
        if !seen_models.insert(model.id.as_str()) {
            issues.push(format!(
                "{file}: provider '{}' declares model '{}' twice",
                data.name, model.id
            ));
        }
        if !seen_provider_model_ids.insert(model.provider_model_id.as_str()) {
            issues.push(format!(
                "{file}: provider '{}' declares provider_model_id '{}' twice",
                data.name, model.provider_model_id
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
        // A provider may serve ids beyond the catalog, so the reserved
        // namespace has to be rejected here too — the advisory above would
        // otherwise let one through without failing validation.
        reject_reserved_namespace(&model.id, &file, issues);
        if let Some(protocols) = &model.api_protocol {
            validate_protocol_list(protocols, &file, "models.api_protocol", issues);
        }
        if let Some(pricing) = &model.pricing {
            validate_pricing(pricing, &file, &model.id, issues);
        }
        if let Some(reasoning_effort) = &model.reasoning_effort {
            if !model.capabilities.contains(&Capability::Reasoning) {
                issues.push(format!(
                    "{file}: model '{}' reasoning_effort requires the reasoning capability",
                    model.id
                ));
            }
            if let Err(error) = reasoning_effort.validate() {
                issues.push(format!(
                    "{file}: model '{}' has invalid reasoning_effort: {error}",
                    model.id
                ));
            }
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
    // Windows forms are rejected on every platform: the validator may run on
    // Unix while the renderer joins these onto a scratch path on Windows,
    // where `..\\evil` and `C:\\evil` both escape.
    let windows_drive = value.len() >= 2
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':';
    let offending = value.is_empty()
        || Path::new(value).is_absolute()
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
                 `bitrouter agents conformance {}/{}`",
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
    /// a `bitrouter launch` facet; absence means the agent is ACP-only.
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
    /// closed set of values. See `docs/AGENT_REGISTRY_SPEC.md` D4 for why this
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    benchmarks: Option<Benchmarks>,
}

/// Independent-benchmark results for a canonical model, keyed by benchmark.
/// Only the benchmarks BitRouter curates on (see `registry/README.md`) get a
/// field here; the shape is extensible to the other curation benchmarks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Benchmarks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_bench_2_1: Option<TerminalBench21>,
}

/// Terminal-Bench 2.1 results — 89 command-line agent tasks, all-or-nothing
/// pytest scoring (Laude Institute / Stanford + the Terminal-Bench community).
///
/// The three metrics are **optional** and stay absent until measured: we
/// populate them from our own runs of the open harness routed through BitRouter
/// (`measured_by: bitrouter`), not from unverified numbers. The provenance
/// fields are first-class on purpose — a benchmark score is meaningless without
/// them, because the same model on the same benchmark version can differ by
/// double-digit points across harness and reasoning-effort. `harness` + `config`
/// pin a run so it is reproducible; `measured_by` + `source_url` keep a
/// third-party citation from ever being mistaken for a BitRouter measurement.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TerminalBench21 {
    /// Score: percent of the 89 tasks passed (`0..=100`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accuracy: Option<f64>,
    /// Average cost per task, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_per_task: Option<f64>,
    /// Average time per task, in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_per_task: Option<f64>,
    /// Who produced the numbers: `bitrouter` for our own runs, otherwise the
    /// third-party source (e.g. `artificial-analysis`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    measured_by: Option<String>,
    /// Agent harness the run used (e.g. `terminus-2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    /// Reasoning-effort / configuration label the run used (e.g. `max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<String>,
    /// Citation URL when `measured_by` is a third party.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    /// Snapshot date (`YYYY-MM-DD`) the numbers were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    as_of: Option<String>,
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
    status: EntryStatus,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compatibility: Option<ModelCompatibility>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffortConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deprecation_date: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCompatibility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_completions: Option<ChatCompletionsCompatibility>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChatCompletionsCompatibility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_limit_field: Option<ChatTokenLimitField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supports_store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supports_stream_options: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChatTokenLimitField {
    MaxTokens,
    MaxCompletionTokens,
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

/// Lifecycle gate shared by provider and runtime entries: only `active` is
/// served. `staging` marks an entry scaffolded from research but not yet
/// confirmed against the live API (or, for a runtime agent, not yet exercised
/// by the conformance suite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum EntryStatus {
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

    /// The pin rule is the security contract of `registry/runtimes/`: the
    /// catalog is fetched over the network and names commands BitRouter
    /// spawns, so a floating tag means the fetched document chooses which code
    /// runs. Scoped npm names lead with `@`, which is the case that makes a
    /// naive `split_once('@')` wrong.
    fn conformance(handshake: TierOutcome, measured_by: &str) -> ConformanceRecord {
        ConformanceRecord {
            handshake: Some(handshake),
            routability: Some(TierOutcome::Pass),
            lifecycle: None,
            suite_version: "1.0.0".to_string(),
            agent_version: Some("0.70.0".to_string()),
            measured_by: measured_by.to_string(),
            as_of: "2026-09-06".to_string(),
            source_url: None,
        }
    }

    /// The gate that makes a conformance record mean something: an agent whose
    /// own record says a tier failed cannot be served by an active runtime.
    #[test]
    fn an_active_runtime_cannot_serve_an_agent_whose_record_reports_failure() {
        let mut issues = Vec::new();
        validate_conformance(
            &conformance(TierOutcome::Fail, "bitrouter"),
            "claude-acp",
            EntryStatus::Active,
            "file",
            &mut issues,
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("handshake: fail"), "{issues:?}");

        // The same record under a staging runtime is the whole point of
        // staging: it records what was observed without serving it.
        let mut staged = Vec::new();
        validate_conformance(
            &conformance(TierOutcome::Fail, "bitrouter"),
            "claude-acp",
            EntryStatus::Staging,
            "file",
            &mut staged,
        );
        assert!(staged.is_empty(), "{staged:?}");
    }

    /// A cited third-party result must be checkable, or it is a claim wearing
    /// a record's clothes.
    #[test]
    fn a_third_party_conformance_record_must_cite_a_source() {
        let mut issues = Vec::new();
        validate_conformance(
            &conformance(TierOutcome::Pass, "some-vendor"),
            "claude-acp",
            EntryStatus::Active,
            "file",
            &mut issues,
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("source_url"), "{issues:?}");

        // Our own runs need no citation.
        let mut ours = Vec::new();
        validate_conformance(
            &conformance(TierOutcome::Pass, "bitrouter"),
            "claude-acp",
            EntryStatus::Active,
            "file",
            &mut ours,
        );
        assert!(ours.is_empty(), "{ours:?}");
    }

    #[test]
    fn package_pinning_reads_the_version_not_the_npm_scope() {
        assert!(package_spec_is_pinned(
            "@agentclientprotocol/claude-agent-acp@0.70.0"
        ));
        assert!(package_spec_is_pinned("pi-acp@1.2.3"));
        assert!(!package_spec_is_pinned("@google/gemini-cli@latest"));
        assert!(!package_spec_is_pinned("pi-acp@latest"));
        assert!(!package_spec_is_pinned("pi-acp@next"));
        // Ranges and dist-tags hand the choice of which code runs back to the
        // registry document just as `@latest` does, so the rule tests for
        // exact semver rather than screening a list of known-floating tags —
        // a denylist would let every one of these through.
        for floating in [
            "pi-acp@^1.0.0",
            "pi-acp@~1.2.3",
            "pi-acp@1.x",
            "pi-acp@*",
            "pi-acp@>=1.0.0",
            "pi-acp@1.2",
            "pi-acp@stable",
            "pi-acp@nightly",
        ] {
            assert!(!package_spec_is_pinned(floating), "{floating} is not a pin");
        }
        // Pre-release and build metadata are still exact.
        assert!(package_spec_is_pinned("pi-acp@1.2.3-rc.1"));
        assert!(package_spec_is_pinned("pi-acp@1.2.3+build.5"));
        // No version at all — `npx` would resolve whatever is current.
        assert!(!package_spec_is_pinned("pi-acp"));
        // A scope with no version must not read as `scope@name`.
        assert!(!package_spec_is_pinned("@agentclientprotocol/codex-acp"));
    }

    #[test]
    fn package_spec_skips_runner_flags_and_the_separator() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            package_spec(&args(&["-y", "--", "@google/gemini-cli@1.0.0", "--acp"])),
            Some("@google/gemini-cli@1.0.0")
        );
        assert_eq!(package_spec(&args(&["-y"])), None);
    }

    #[test]
    fn routing_placeholders_are_checked_against_the_context_they_appear_in() {
        let check = |value: &str, allowed: &[&str]| {
            let mut issues = Vec::new();
            validate_placeholders("agent", "field", value, allowed, "file", &mut issues);
            issues
        };
        assert!(check("{base_url_v1}", SKELETON_PLACEHOLDERS).is_empty());
        assert!(check("{dir}", ENV_PLACEHOLDERS).is_empty());
        assert!(check("no placeholders here", ENV_PLACEHOLDERS).is_empty());

        // `{dir}` only exists once the file has a path — it is not a skeleton
        // placeholder, and using it there must not silently render literally.
        let wrong_context = check("{dir}", SKELETON_PLACEHOLDERS);
        assert_eq!(wrong_context.len(), 1, "{wrong_context:?}");
        assert!(wrong_context[0].contains("unknown placeholder"));

        assert_eq!(check("{secret}", ENV_PLACEHOLDERS).len(), 1);
        assert_eq!(check("{dir", ENV_PLACEHOLDERS).len(), 1);
    }

    #[test]
    fn config_paths_must_stay_inside_the_per_launch_directory() {
        let check = |value: &str| {
            let mut issues = Vec::new();
            validate_relative_path("agent", "routing.file", value, "file", &mut issues);
            issues
        };
        assert!(check("opencode.json").is_empty());
        assert!(check("pi-agent/models.json").is_empty());
        assert_eq!(check("../../etc/profile").len(), 1);
        assert_eq!(check("/etc/profile").len(), 1);
        assert_eq!(check("").len(), 1);
    }

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
    fn canonical_model_parses_terminal_bench_and_validates() {
        let yaml = r#"
id: acme/one
name: "Acme: One"
input_modalities: [text]
output_modalities: [text]
benchmarks:
  terminal_bench_2_1:
    accuracy: 61.3
    cost_per_task: 0.75
    time_per_task: 4.35
    measured_by: bitrouter
    harness: terminus-2
    config: max
    as_of: 2026-07-17
"#;
        let model: CanonicalModel =
            serde_saphyr::from_str(yaml).expect("parses terminal_bench_2_1 block");
        let mut issues = Vec::new();
        validate_canonical_model(&model, &mut issues);
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");

        // Filled metrics survive the round-trip; unset fields are omitted.
        let dist = serde_json::to_value(&model).unwrap();
        let tb = &dist["benchmarks"]["terminal_bench_2_1"];
        assert_eq!(tb["accuracy"], json!(61.3));
        assert_eq!(tb["measured_by"], json!("bitrouter"));
        assert!(
            tb.get("source_url").is_none(),
            "unset field must be omitted"
        );
    }

    #[test]
    fn canonical_model_terminal_bench_metrics_are_optional() {
        // The shape ships now with metrics unfilled — this must parse + validate,
        // and must not invent any metric into the dist output.
        let yaml = r#"
id: acme/two
input_modalities: [text]
output_modalities: [text]
benchmarks:
  terminal_bench_2_1: {}
"#;
        let model: CanonicalModel =
            serde_saphyr::from_str(yaml).expect("empty terminal_bench_2_1 parses");
        let mut issues = Vec::new();
        validate_canonical_model(&model, &mut issues);
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        let dist = serde_json::to_value(&model).unwrap();
        assert_eq!(dist["benchmarks"]["terminal_bench_2_1"], json!({}));
    }

    #[test]
    fn canonical_model_rejects_out_of_range_terminal_bench() {
        let yaml = r#"
id: acme/three
input_modalities: [text]
output_modalities: [text]
benchmarks:
  terminal_bench_2_1:
    accuracy: 142.0
    cost_per_task: -1.0
    as_of: 07-2026
"#;
        let model: CanonicalModel = serde_saphyr::from_str(yaml).expect("parses");
        let mut issues = Vec::new();
        validate_canonical_model(&model, &mut issues);
        assert_eq!(
            issues.len(),
            3,
            "expected accuracy + cost + as_of issues: {issues:?}"
        );
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
    fn canonical_model_cannot_use_the_reserved_bitrouter_namespace() {
        let root = test_root("reserved-canonical-model");
        write(
            &root,
            "registry/models/bitrouter.yaml",
            r#"
- id: bitrouter/physical-model
  name: "BitRouter: Physical Model"
  input_modalities: [text]
  output_modalities: [text]
"#,
        );

        let loaded = load_registry(&root).expect("loads");
        let err = validate_loaded(&loaded).expect_err("reserved catalog model must fail");
        assert!(
            err.to_string().contains("reserved 'bitrouter/' namespace"),
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
    capabilities: [reasoning]
    reasoning_effort:
      levels: [low, medium, high]
      default: high
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
            provider["models"][0]["reasoning_effort"],
            json!({ "levels": ["low", "medium", "high"], "default": "high" })
        );
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
            compatibility: None,
            capabilities: Vec::new(),
            reasoning_effort: None,
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
    fn models_dev_catalog_preserves_existing_provider_model_mapping() {
        let provider: ProviderFile = serde_saphyr::from_str(
            r#"
name: deepseek
api_protocol:
  - "*": openai
models:
  - id: deepseek/deepseek-v4-flash-0731
    provider_model_id: deepseek-v4-flash
status: active
billing: usage_token
api_base: https://api.deepseek.test/v1
auto_sync:
  feed: models_dev
"#,
        )
        .unwrap();
        let catalog: ModelsDevProvider = serde_json::from_str(
            r#"{"models":{"deepseek-v4-flash":{"cost":{"input":0.14,"output":0.28}}}}"#,
        )
        .unwrap();
        let resolve = canonical_resolver([
            "deepseek/deepseek-v4-flash",
            "deepseek/deepseek-v4-flash-0731",
        ]);

        let adds = models_dev_plan_for_provider(&provider, &catalog, &resolve);

        assert!(
            adds.is_empty(),
            "an explicitly mapped upstream model must not be attached to a second canonical ID"
        );
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
    fn v1_models_catalog_preserves_existing_provider_model_mapping() {
        let provider: ProviderFile = serde_saphyr::from_str(
            r#"
name: deepseek
api_protocol:
  - "*": openai
models:
  - id: deepseek/deepseek-v4-flash-0731
    provider_model_id: deepseek-v4-flash
status: active
billing: subscription
api_base: https://api.deepseek.test/v1
auto_sync:
  feed: v1_models
"#,
        )
        .unwrap();
        let resolve = canonical_resolver([
            "deepseek/deepseek-v4-flash",
            "deepseek/deepseek-v4-flash-0731",
        ]);
        let body = r#"{"data":[{"id":"deepseek-v4-flash"}]}"#;

        let plan = v1_models_plan_for_provider(&provider, body, &resolve).unwrap();

        assert!(
            plan.adds.is_empty(),
            "an explicitly mapped upstream model must not be attached to a second canonical ID"
        );
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

    #[cfg(unix)]
    #[test]
    fn registry_sync_workflow_skips_empty_agentic_feeds() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let workflow = include_str!("../../../.github/workflows/registry-sync.yml");
        let marker = "      - name: Sync agentic registry feeds\n";
        let (_, after_marker) = workflow
            .split_once(marker)
            .ok_or_else(|| std::io::Error::other("missing agentic sync workflow step"))?;
        let (_, after_run) = after_marker
            .split_once("        run: |\n")
            .ok_or_else(|| std::io::Error::other("missing agentic sync run block"))?;
        let mut script = String::new();
        for line in after_run.lines() {
            if line.starts_with("      - ") {
                break;
            }
            if line.is_empty() {
                script.push('\n');
            } else {
                let command = line.strip_prefix("          ").ok_or_else(|| {
                    std::io::Error::other("unexpected agentic sync run-block indentation")
                })?;
                script.push_str(command);
                script.push('\n');
            }
        }

        let root = test_root("empty-agentic-workflow");
        fs::create_dir_all(root.join("target"))?;
        write(
            &root,
            "bin/cargo",
            r#"#!/bin/sh
cat <<'EOF'
Providers to sync:

(No `auto_sync.feed: agentic` providers are configured.)

Canonical model source:
- `registry/models/<vendor>.yaml` is curated.
EOF
"#,
        );
        write(&root, "bin/npm", "#!/bin/sh\nexit 42\n");
        for command in ["bin/cargo", "bin/npm"] {
            let path = root.join(command);
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions)?;
        }

        let path = format!("{}:{}", root.join("bin").display(), std::env::var("PATH")?);
        let output = Command::new("bash")
            .args(["-euo", "pipefail", "-c", &script])
            .current_dir(&root)
            .env("PATH", path)
            .env("BITROUTER_API_KEY", "test-only")
            .output()?;

        assert!(
            output.status.success(),
            "workflow should skip before npm; status={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "no agentic registry providers configured"
        );
        Ok(())
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
    fn provider_declared_model_cannot_use_the_reserved_bitrouter_namespace() {
        let root = test_root("reserved-provider-model");
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
    pricing:
      input_tokens:
        no_cache: 1.0
      output_tokens:
        text: 2.0
  - id: bitrouter/provider-only
    provider_model_id: provider-only
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
        let err = validate_loaded(&loaded).expect_err("reserved provider model must fail");
        assert!(
            err.to_string().contains("reserved 'bitrouter/' namespace"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn provider_rejects_duplicate_provider_model_ids() {
        let root = test_root("duplicate-provider-model-id");
        write(
            &root,
            "registry/models/acme.yaml",
            r#"
- id: acme/preview
  name: "Acme: Preview"
  input_modalities: [text]
  output_modalities: [text]
- id: acme/release
  name: "Acme: Release"
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
  - id: acme/preview
    provider_model_id: flash
  - id: acme/release
    provider_model_id: flash
status: active
billing: subscription
api_base: https://api.acme.test/v1
"#,
        );

        let loaded = load_registry(&root).expect("loads");
        let err = validate_loaded(&loaded).expect_err("duplicate upstream IDs must fail");

        assert!(
            err.to_string().contains("provider_model_id 'flash' twice"),
            "unexpected error: {err}"
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

    #[test]
    fn built_registry_maps_configured_provider_ids_for_recovered_models() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifacts = build_artifacts(&root).expect("builds repository registry");
        let providers: Value =
            serde_json::from_str(&artifacts.providers).expect("valid providers JSON");
        let empty = Vec::new();
        let provider_data = providers["data"].as_array();
        assert!(provider_data.is_some(), "provider data array");
        let provider_data = provider_data.unwrap_or(&empty);

        let gmicloud = provider_data
            .iter()
            .find(|provider| provider["name"] == "gmicloud");
        assert!(gmicloud.is_some(), "GMI Cloud provider");
        assert_provider_mapping(
            gmicloud,
            "GMI Cloud",
            "qwen/qwen3.7-max",
            "Qwen/Qwen3.7-Max",
            2.5,
            (Some(0.25), Some(3.125)),
            7.5,
        );

        let siliconflow = provider_data
            .iter()
            .find(|provider| provider["name"] == "siliconflow");
        assert!(siliconflow.is_some(), "SiliconFlow provider");
        assert_provider_mapping(
            siliconflow,
            "SiliconFlow",
            "deepseek/deepseek-v4-pro",
            "deepseek-ai/DeepSeek-V4-Pro",
            1.74,
            (Some(0.145), None),
            3.48,
        );
    }

    #[test]
    fn checked_in_registry_pins_the_official_effort_matrix() -> Result<()> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let loaded = load_registry(&root)?;
        validate_loaded(&loaded)?;
        let cases: &[(&str, &str, &[&str], &str)] = &[
            (
                "openai",
                "openai/gpt-5.6-sol",
                &["none", "low", "medium", "high", "xhigh", "max"],
                "medium",
            ),
            (
                "openai",
                "openai/gpt-5.5",
                &["none", "low", "medium", "high", "xhigh"],
                "medium",
            ),
            (
                "openai-codex",
                "openai/gpt-5.4",
                &["none", "low", "medium", "high", "xhigh"],
                "none",
            ),
            (
                "anthropic",
                "anthropic/claude-opus-4.8",
                &["low", "medium", "high", "xhigh", "max"],
                "high",
            ),
            (
                "claude-code",
                "anthropic/claude-opus-4.6",
                &["low", "medium", "high", "max"],
                "high",
            ),
            (
                "google",
                "google/gemini-3.1-pro-preview",
                &["low", "medium", "high"],
                "high",
            ),
            (
                "google",
                "google/gemini-3.5-flash",
                &["minimal", "low", "medium", "high"],
                "medium",
            ),
            (
                "bitrouter",
                "openai/gpt-5.6-sol",
                &["none", "low", "medium", "high", "xhigh", "max"],
                "medium",
            ),
        ];

        for (provider_name, model_id, expected_levels, expected_default) in cases {
            let provider = loaded
                .providers
                .iter()
                .find(|provider| provider.data.name == *provider_name)
                .ok_or_else(|| anyhow::anyhow!("missing provider {provider_name}"))?;
            let model = provider
                .data
                .models
                .iter()
                .find(|model| model.id == *model_id)
                .ok_or_else(|| anyhow::anyhow!("missing route {provider_name}:{model_id}"))?;
            let effort = model.reasoning_effort.as_ref().ok_or_else(|| {
                anyhow::anyhow!("missing effort matrix for {provider_name}:{model_id}")
            })?;
            let actual_levels = effort
                .levels
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                actual_levels, *expected_levels,
                "{provider_name}:{model_id}"
            );
            assert_eq!(
                effort.default.map(|value| value.as_str()),
                Some(*expected_default),
                "{provider_name}:{model_id}"
            );
        }
        for (provider_name, model_id) in [
            ("anthropic", "anthropic/claude-sonnet-4.5"),
            ("google", "google/gemini-3.1-flash-lite-preview"),
        ] {
            let provider = loaded
                .providers
                .iter()
                .find(|provider| provider.data.name == provider_name)
                .ok_or_else(|| anyhow::anyhow!("missing provider {provider_name}"))?;
            let model = provider
                .data
                .models
                .iter()
                .find(|model| model.id == model_id)
                .ok_or_else(|| anyhow::anyhow!("missing route {provider_name}:{model_id}"))?;
            assert!(
                model.reasoning_effort.is_none(),
                "unverified route must not advertise effort support: {provider_name}:{model_id}"
            );
        }
        Ok(())
    }

    #[test]
    fn built_registry_separates_deepseek_v4_flash_revisions() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifacts = build_artifacts(&root).expect("builds repository registry");
        let models: Value = serde_json::from_str(&artifacts.models).expect("valid models JSON");
        let providers: Value =
            serde_json::from_str(&artifacts.providers).expect("valid providers JSON");

        let dated = models["data"].as_array().and_then(|models| {
            models
                .iter()
                .find(|model| model["id"] == "deepseek/deepseek-v4-flash-0731")
        });
        assert!(dated.is_some(), "dated canonical model");
        let Some(dated) = dated else {
            return;
        };
        assert_eq!(dated["name"], "DeepSeek: DeepSeek V4 Flash 0731");
        assert_eq!(
            dated["description"],
            "Official DeepSeek V4 Flash release with enhanced agentic capabilities."
        );
        assert_eq!(dated["input_modalities"], serde_json::json!(["text"]));
        assert_eq!(dated["output_modalities"], serde_json::json!(["text"]));
        assert_eq!(dated["release_date"], "2026-07-31");
        assert_eq!(dated["max_input_tokens"], 1_000_000);
        assert_eq!(dated["max_output_tokens"], 384_000);
        assert_eq!(dated["knowledge_cutoff"], "2025-05");
        assert_eq!(dated["open_weights"], true);
        assert_eq!(dated["family"], "deepseek-flash");

        let preview = models["data"].as_array().and_then(|models| {
            models
                .iter()
                .find(|model| model["id"] == "deepseek/deepseek-v4-flash")
        });
        assert!(preview.is_some(), "preview canonical model");
        let Some(preview) = preview else {
            return;
        };
        assert_eq!(preview["name"], "DeepSeek: DeepSeek V4 Flash");
        assert!(preview.get("description").is_none());
        assert_eq!(preview["input_modalities"], serde_json::json!(["text"]));
        assert_eq!(preview["output_modalities"], serde_json::json!(["text"]));
        assert_eq!(preview["release_date"], "2026-04-24");
        assert_eq!(preview["max_input_tokens"], 262_144);
        assert_eq!(preview["max_output_tokens"], 262_144);
        assert_eq!(preview["knowledge_cutoff"], "2025-05");
        assert_eq!(preview["open_weights"], true);
        assert_eq!(preview["family"], "deepseek-flash");

        let provider_data = providers["data"].as_array();
        assert!(provider_data.is_some(), "provider data array");
        let Some(provider_data) = provider_data else {
            return;
        };
        let find_mapping = |provider_name: &str, canonical_id: &str| {
            provider_data
                .iter()
                .find(|provider| provider["name"] == provider_name)
                .and_then(|provider| provider["models"].as_array())
                .and_then(|models| models.iter().find(|model| model["id"] == canonical_id))
        };

        let expected = [
            ("deepseek", "deepseek-v4-flash"),
            ("opencode-zen", "deepseek-v4-flash"),
            ("opencode-go", "deepseek-v4-flash"),
            ("alibaba_cn", "deepseek-v4-flash-0731"),
            ("ambient", "deepseek/deepseek-v4-flash-0731"),
            ("atlascloud", "deepseek-ai/deepseek-v4-flash-0731"),
            ("novita", "deepseek/deepseek-v4-flash-0731"),
            ("openrouter", "deepseek/deepseek-v4-flash-0731"),
            ("qianfan", "deepseek-v4-flash-0731"),
        ];
        for (provider_name, provider_model_id) in expected {
            let mapping = find_mapping(provider_name, "deepseek/deepseek-v4-flash-0731");
            assert!(
                mapping.is_some(),
                "{provider_name} should serve the dated canonical model"
            );
            assert_eq!(
                mapping.and_then(|model| model["provider_model_id"].as_str()),
                Some(provider_model_id),
                "{provider_name} upstream model ID"
            );
        }

        for provider_name in ["deepseek", "opencode-zen", "opencode-go"] {
            assert!(
                find_mapping(provider_name, "deepseek/deepseek-v4-flash").is_none(),
                "{provider_name} no longer serves the preview alias"
            );
        }
        for provider_name in [
            "alibaba_cn",
            "ambient",
            "atlascloud",
            "novita",
            "openrouter",
            "qianfan",
        ] {
            assert!(
                find_mapping(provider_name, "deepseek/deepseek-v4-flash").is_some(),
                "{provider_name} keeps its distinct preview model"
            );
        }

        let deepseek = find_mapping("deepseek", "deepseek/deepseek-v4-flash-0731");
        assert_eq!(
            deepseek.map(|model| model["api_protocol"].clone()),
            Some(serde_json::json!(["openai", "responses", "anthropic"]))
        );

        let openrouter = find_mapping("openrouter", "deepseek/deepseek-v4-flash-0731");
        assert_eq!(
            openrouter.and_then(|model| model["pricing"]["input_tokens"]["no_cache"].as_f64()),
            Some(0.09)
        );
        assert_eq!(
            openrouter.and_then(|model| model["pricing"]["input_tokens"]["cache_read"].as_f64()),
            Some(0.018)
        );
        assert_eq!(
            openrouter.and_then(|model| model["pricing"]["output_tokens"]["text"].as_f64()),
            Some(0.18)
        );
    }

    #[test]
    fn built_registry_refreshes_qianfan_international_catalog() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifacts = build_artifacts(&root).expect("builds repository registry");
        let providers: Value =
            serde_json::from_str(&artifacts.providers).expect("valid providers JSON");
        let qianfan = providers["data"]
            .as_array()
            .expect("provider data array")
            .iter()
            .find(|provider| provider["name"] == "qianfan");

        assert!(qianfan.is_some(), "Qianfan International provider");
        assert_provider_mapping(
            qianfan,
            "Qianfan International",
            "deepseek/deepseek-v4-pro",
            "deepseek-v4-pro",
            1.69,
            (Some(0.14), None),
            3.38,
        );
        assert_provider_mapping(
            qianfan,
            "Qianfan International",
            "z-ai/glm-5.2",
            "glm-5.2",
            1.4,
            (Some(0.26), None),
            4.4,
        );
    }

    #[test]
    fn built_registry_uses_current_bitrouter_cloud_kimi_pricing() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifacts = build_artifacts(&root).expect("builds repository registry");
        let providers: Value =
            serde_json::from_str(&artifacts.providers).expect("valid providers JSON");
        let bitrouter = providers["data"]
            .as_array()
            .expect("provider data array")
            .iter()
            .find(|provider| provider["name"] == "bitrouter");

        assert!(bitrouter.is_some(), "BitRouter Cloud provider");
        assert_provider_mapping(
            bitrouter,
            "BitRouter Cloud",
            "moonshotai/kimi-k2.7-code",
            "moonshotai/kimi-k2.7-code",
            0.7125,
            (Some(0.1425), None),
            3.0,
        );
    }

    fn assert_provider_mapping(
        provider: Option<&Value>,
        provider_name: &str,
        canonical_id: &str,
        provider_model_id: &str,
        input_price: f64,
        cache_prices: (Option<f64>, Option<f64>),
        output_price: f64,
    ) {
        let model = provider
            .and_then(|provider| provider["models"].as_array())
            .and_then(|models| models.iter().find(|model| model["id"] == canonical_id));

        assert!(
            model.is_some(),
            "{provider_name} mapping for {canonical_id}"
        );
        assert_eq!(
            model.and_then(|model| model["id"].as_str()),
            Some(canonical_id)
        );
        assert_eq!(
            model.and_then(|model| model["provider_model_id"].as_str()),
            Some(provider_model_id)
        );
        assert_eq!(
            model.and_then(|model| model["pricing"]["input_tokens"]["no_cache"].as_f64()),
            Some(input_price)
        );
        let input_tokens = model
            .and_then(|model| model["pricing"]["input_tokens"].as_object())
            .expect("input token pricing");
        match cache_prices.0 {
            Some(cache_read_price) => {
                assert_eq!(
                    input_tokens.get("cache_read").and_then(Value::as_f64),
                    Some(cache_read_price)
                );
            }
            None => {
                assert!(
                    !input_tokens.contains_key("cache_read"),
                    "{provider_name} {canonical_id} should not advertise cache-read pricing"
                );
            }
        }
        match cache_prices.1 {
            Some(cache_write_price) => {
                assert_eq!(
                    input_tokens.get("cache_write").and_then(Value::as_f64),
                    Some(cache_write_price)
                );
            }
            None => {
                assert!(
                    !input_tokens.contains_key("cache_write"),
                    "{provider_name} {canonical_id} should not advertise cache-write pricing"
                );
            }
        }
        assert_eq!(
            model.and_then(|model| model["pricing"]["output_tokens"]["text"].as_f64()),
            Some(output_price)
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
