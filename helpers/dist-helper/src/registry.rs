//! Registry v1 source transactions and deterministic public artifacts.

use registry_json as serde_json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bitrouter_registry::document::{
    Document, ReaderPolicy, SCHEMA, SPEC_REVISION, canonical_id, integer, timestamp,
};
use bitrouter_registry::{feed, source};
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod agents;
mod discovery;

#[derive(Clone)]
struct Sources {
    files: BTreeMap<PathBuf, Value>,
    manifest: PathBuf,
    identities: PathBuf,
    tree_digest: String,
}
impl Sources {
    fn load(root: &Path) -> Result<Self> {
        let mut paths = Vec::new();
        yaml_files(&root.join("registry/models"), &mut paths)?;
        yaml_files(&root.join("registry/providers"), &mut paths)?;
        let manifest = root.join("registry/registry.yaml");
        let identities = root.join("registry/model-identities.yaml");
        paths.extend([
            manifest.clone(),
            identities.clone(),
            root.join("registry/model-discovery.yaml"),
        ]);
        let files = paths
            .into_iter()
            .map(|path| {
                Ok((
                    path.clone(),
                    source::parse(
                        &fs::read_to_string(&path)
                            .with_context(|| format!("reading {}", path.display()))?,
                    )?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            files,
            manifest,
            identities,
            tree_digest: tree_digest(&root.join("registry"))?,
        })
    }
    fn records(&self, directory: &str) -> impl Iterator<Item = (&PathBuf, &Value)> {
        self.files.iter().filter(move |(path, _)| {
            path.parent()
                .and_then(Path::file_name)
                .is_some_and(|v| v == directory)
        })
    }
    fn document(&self) -> Result<Document> {
        let policy = self
            .manifest
            .parent()
            .context("registry parent")?
            .join("model-discovery.yaml");
        discovery::validate(&self.files[&policy])?;
        let mut value = self.files[&self.manifest].clone();
        let root = value
            .as_object_mut()
            .context("registry manifest must be an object")?;
        root.remove("identity_source");
        let as_of = root
            .get("as_of")
            .and_then(Value::as_str)
            .and_then(timestamp)
            .context("manifest needs UTC as_of")?;
        let mut models = Vec::new();
        for (path, records) in self.records("models") {
            for model in records
                .as_array()
                .context("curated source must be a sequence")?
            {
                let id = model["id"].as_str().context("curated ID missing")?;
                if id.split_once('/').map(|p| p.0) != path.file_stem().and_then(|s| s.to_str()) {
                    bail!("curated namespace must match filename: {}", path.display());
                }
                if model
                    .pointer("/curation/expires_at")
                    .and_then(Value::as_str)
                    .and_then(timestamp)
                    .is_some_and(|t| t <= as_of)
                {
                    continue;
                }
                models.push(model.clone());
            }
        }
        let mut providers = Vec::new();
        for (path, record) in self.records("providers") {
            if record["name"].as_str() != path.file_stem().and_then(|s| s.to_str()) {
                bail!("provider name must match filename: {}", path.display());
            }
            validate_feeds(record)?;
            let value = public_provider(record)?;
            providers.push(value);
        }
        models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        providers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        for provider in &mut providers {
            provider["models"]
                .as_array_mut()
                .context("models must be array")?
                .sort_by(|a, b| {
                    a["provider_model_id"]
                        .as_str()
                        .cmp(&b["provider_model_id"].as_str())
                });
        }
        let data = root
            .entry("data")
            .or_insert(json!({}))
            .as_object_mut()
            .context("manifest data must be object")?;
        data.insert("models".into(), json!(models));
        data.insert("providers".into(), json!(providers));
        let document = Document::from_value(value, &ReaderPolicy::default())?;
        for model in document.models() {
            if !available(&document, model["id"].as_str().unwrap_or_default()) {
                bail!("curated model has no eligible offer: {}", model["id"]);
            }
        }
        validate_bindings(&document, &self.files[&self.identities], None)?;
        Ok(document)
    }
    fn digest(&self) -> Result<String> {
        let mut hash = Sha256::new();
        for (path, value) in &self.files {
            // Stable repository-relative path, never a machine-dependent prefix.
            let relative =
                path.strip_prefix(self.manifest.parent().context("manifest parent missing")?)?;
            hash.update(relative.to_string_lossy().as_bytes());
            hash.update([0]);
            hash.update(source::render(value)?);
            hash.update([0]);
        }
        Ok(hex::encode(hash.finalize()))
    }
    fn publish(&self, original: &Self, outputs: &[(PathBuf, Vec<u8>)]) -> Result<()> {
        self.publish_with(original, outputs, |_| Ok(()))
    }
    fn publish_with(
        &self,
        original: &Self,
        outputs: &[(PathBuf, Vec<u8>)],
        mut before_output: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        let registry = self
            .manifest
            .parent()
            .context("registry directory missing")?;
        let root = registry.parent().context("repository root missing")?;
        let transaction = root.join(".registry-publication");
        if transaction.exists() {
            bail!("unfinished registry publication; recover before publishing");
        }
        fs::create_dir(&transaction)?;
        let prepared = (|| {
            let staged = transaction.join("candidate");
            copy_tree(registry, &staged)?;
            for (path, value) in &self.files {
                if original.files.get(path) != Some(value) {
                    atomic_write(
                        &staged.join(path.strip_prefix(registry)?),
                        source::render(value)?.as_bytes(),
                    )?;
                }
            }
            let mut paths = Vec::new();
            for (path, bytes) in outputs {
                let relative = path.strip_prefix(root)?;
                valid_output_path(relative)?;
                atomic_write(&transaction.join("output-new").join(relative), bytes)?;
                match fs::read(path) {
                    Ok(prior) => {
                        atomic_write(&transaction.join("output-previous").join(relative), &prior)?
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                paths.push(relative.to_string_lossy().into_owned());
            }
            atomic_write(
                &transaction.join("journal.json"),
                &serde_json::to_vec(&json!({"outputs":paths}))?,
            )?;
            if tree_digest(registry)? != original.tree_digest {
                bail!("registry source changed during preparation; no files published");
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = prepared {
            fs::remove_dir_all(&transaction)?;
            return Err(error);
        }
        let publish = (|| {
            fs::rename(registry, transaction.join("previous"))?;
            fs::rename(transaction.join("candidate"), registry)?;
            sync_directory(root)?;
            for (index, (path, bytes)) in outputs.iter().enumerate() {
                before_output(index)?;
                atomic_write(path, bytes)?;
            }
            atomic_write(&transaction.join("committed"), b"complete\n")?;
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = publish {
            recover_publication(root)
                .context("publication failed and automatic rollback needs recovery")?;
            return Err(error);
        }
        fs::remove_dir_all(transaction)?;
        Ok(())
    }
}

fn available(document: &Document, id: &str) -> bool {
    document.providers().iter().any(|p| {
        p["status"] == "active"
            && p["models"].as_array().is_some_and(|models| {
                models.iter().any(|m| {
                    m["canonical_id"] == id
                        && m["status"] == "active"
                        && m.get("available_until")
                            .and_then(Value::as_str)
                            .and_then(timestamp)
                            .is_none_or(|t| t > document.as_of())
                        && m["interfaces"].as_array().is_some_and(|i| !i.is_empty())
                })
            })
    })
}
fn yaml_files(dir: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for item in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = item?.path();
        if path.is_dir() {
            bail!("model/provider source files must be directly inside their declared directory");
        } else if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yaml" | "yml")
        ) {
            output.push(path);
        }
    }
    output.sort();
    Ok(())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("output parent missing")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("registry source symlinks are unsupported");
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        } else if kind.is_file() {
            let destination = to.join(entry.file_name());
            fs::copy(entry.path(), &destination)?;
            fs::File::open(destination)?.sync_all()?;
        }
    }
    sync_directory(to)
}
fn valid_output_path(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("unsafe publication output path");
    }
    if !(path.starts_with("dist/registry")
        || path.starts_with("apps/bitrouter/registry-dist")
        || path == Path::new("docs/specs/registry-v1/registry.schema.json"))
    {
        bail!("publication output outside registry artifacts");
    }
    Ok(())
}
fn tree_digest(root: &Path) -> Result<String> {
    fn walk(root: &Path, path: &Path, hash: &mut Sha256, depth: usize) -> Result<()> {
        if depth > 64 {
            bail!("source directory nesting exceeds supported depth");
        }
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for path in entries {
            let kind = fs::symlink_metadata(&path)?.file_type();
            if kind.is_symlink() {
                bail!("registry source symlinks are unsupported");
            }
            hash.update(path.strip_prefix(root)?.to_string_lossy().as_bytes());
            hash.update([0]);
            if kind.is_dir() {
                walk(root, &path, hash, depth + 1)?;
            } else {
                hash.update(fs::read(&path)?);
            }
            hash.update([0]);
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    walk(root, root, &mut hash, 0)?;
    Ok(hex::encode(hash.finalize()))
}
fn recover_publication(root: &Path) -> Result<()> {
    let transaction = root.join(".registry-publication");
    if !transaction.exists() {
        return Ok(());
    }
    if !transaction.join("committed").is_file() {
        let previous = transaction.join("previous");
        if previous.exists() {
            if !previous.join("registry.yaml").is_file() {
                bail!("publication recovery has no valid previous source");
            }
            if root.join("registry").exists() {
                fs::remove_dir_all(root.join("registry"))?;
            }
            fs::rename(previous, root.join("registry"))?;
            sync_directory(root)?;
        }
        if transaction.join("journal.json").is_file() {
            let journal = bitrouter_registry::document::parse_json(&fs::read(
                transaction.join("journal.json"),
            )?)?;
            for path in journal["outputs"]
                .as_array()
                .context("invalid publication journal")?
            {
                let relative =
                    Path::new(path.as_str().context("invalid publication journal path")?);
                valid_output_path(relative)?;
                match fs::read(transaction.join("output-previous").join(relative)) {
                    Ok(bytes) => atomic_write(&root.join(relative), &bytes)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        match fs::remove_file(root.join(relative)) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    fs::remove_dir_all(transaction)?;
    Ok(())
}
fn rendered(document: &Document) -> Result<Vec<u8>> {
    let bytes = source::render(document.value())?.into_bytes();
    Document::parse(&bytes, &ReaderPolicy::default())?;
    Ok(bytes)
}
fn output_paths(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("dist/registry/v1/registry.json"),
        root.join(format!("dist/registry/v1/{SPEC_REVISION}/registry.json")),
        root.join("apps/bitrouter/registry-dist/registry.json"),
    ]
}
pub fn validate(root: &Path) -> Result<()> {
    let sources = Sources::load(root)?;
    let document = sources.document()?;
    agents::build(root)?;
    println!(
        "registry valid: {} curated models, {} providers; offline source evidence only",
        document.models().len(),
        document.providers().len()
    );
    Ok(())
}
fn build_outputs(root: &Path, sources: &Sources) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let document = sources.document()?;
    let bytes = rendered(&document)?;
    let mut outputs = output_paths(root)
        .into_iter()
        .map(|path| (path, bytes.clone()))
        .collect::<Vec<_>>();
    let (agents, runtimes) = agents::build(root)?;
    for directory in [
        root.join("dist/registry"),
        root.join("apps/bitrouter/registry-dist"),
    ] {
        outputs.push((directory.join("agents.json"), agents.as_bytes().to_vec()));
        outputs.push((
            directory.join("runtimes.json"),
            runtimes.as_bytes().to_vec(),
        ));
    }
    outputs.push((root.join("apps/bitrouter/registry-dist/source.json"),serde_json::to_vec_pretty(&json!({"source":bitrouter_registry::document::DEFAULT_SOURCE_URL,"version":1,"spec_revision":SPEC_REVISION}))?));
    outputs.push((
        root.join("docs/specs/registry-v1/registry.schema.json"),
        SCHEMA.as_bytes().to_vec(),
    ));
    Ok(outputs)
}
pub fn build(root: &Path, check: bool) -> Result<()> {
    let sources = Sources::load(root)?;
    let document = sources.document()?;
    let bytes = rendered(&document)?;
    let outputs = build_outputs(root, &sources)?;
    for (path, bytes) in outputs {
        if check {
            if fs::read(&path).with_context(|| format!("reading {}", path.display()))? != bytes {
                bail!(
                    "stale registry artifact {}; run registry build",
                    path.display()
                );
            }
        } else {
            atomic_write(&path, &bytes)?;
        }
    }
    println!(
        "registry {}: {} curated models, {} providers, SHA-256 {}",
        if check { "checked" } else { "built" },
        document.models().len(),
        document.providers().len(),
        hex::encode(Sha256::digest(&bytes))
    );
    Ok(())
}

fn validate_feeds(provider: &Value) -> Result<()> {
    bitrouter_registry::reconcile::validate_provider(provider)?;
    let Some(sync) = provider.get("auto_sync") else {
        return Ok(());
    };
    let feeds = sync["feeds"]
        .as_array()
        .context("auto_sync.feeds must be an array")?;
    let mut ids = BTreeSet::new();
    for feed in feeds {
        feed["kind"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("feed kind must be a nonempty string")?;
        let id = feed["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("feed ID missing")?;
        if !ids.insert(id) {
            bail!("duplicate feed ID");
        }
        // Unknown adapters remain source-preserved but fail a scheduled run.
        if matches!(
            feed["kind"].as_str(),
            Some("v1_models" | "openrouter_models")
        ) {
            for result in [
                bitrouter_registry::mapping::validate(feed),
                feed::validate(feed),
            ] {
                if let Err(error) = result
                    && !matches!(
                        error.code,
                        "registry.sync_unsupported" | "registry.unsupported"
                    )
                {
                    return Err(error.into());
                }
            }
        }
    }
    Ok(())
}
fn https(value: &Value) -> bool {
    value
        .as_str()
        .and_then(|s| reqwest::Url::parse(s).ok())
        .is_some_and(|u| {
            u.scheme() == "https"
                && u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
}
fn validate_bindings(
    document: &Document,
    ledger: &Value,
    current: Option<&BTreeSet<String>>,
) -> Result<Value> {
    let report = binding_report(document, ledger, current)?;
    if report["outcome"] != "passed" {
        bail!(
            "identity validation failed: {}",
            serde_json::to_string(&report["mismatches"])?
        );
    }
    Ok(report)
}
fn binding_report(
    document: &Document,
    ledger: &Value,
    current: Option<&BTreeSet<String>>,
) -> Result<Value> {
    let entries = ledger
        .as_array()
        .context("identity ledger must be a sequence")?;
    let mut ids = BTreeSet::new();
    let mut bindings = BTreeMap::new();
    for entry in entries {
        let canonical = entry["canonical_id"]
            .as_str()
            .filter(|s| canonical_id(s))
            .context("invalid ledger canonical ID")?;
        if !ids.insert(canonical)
            || !https(&entry["source_url"])
            || entry["observed_at"].as_str().and_then(timestamp).is_none()
        {
            bail!("invalid or duplicate identity evidence");
        }
        if let Some(values) = entry.get("bindings") {
            for binding in values
                .as_array()
                .context("identity bindings must be array")?
            {
                let provider = binding["provider"]
                    .as_str()
                    .context("binding provider missing")?;
                let native = binding["provider_model_id"]
                    .as_str()
                    .context("binding native ID missing")?;
                if !https(&binding["source_url"])
                    || binding["verified_at"]
                        .as_str()
                        .and_then(timestamp)
                        .is_none()
                    || bindings.insert((provider, native), canonical).is_some()
                {
                    bail!("canonical_binding_conflict: invalid or duplicate binding evidence");
                }
            }
        }
    }
    let mut references = 0;
    let mut distinct = BTreeSet::new();
    let mut unbound = 0;
    let mut mismatches = Vec::new();
    for (i, model) in document.models().iter().enumerate() {
        let id = model["id"].as_str().context("curated ID missing")?;
        references += 1;
        distinct.insert(id);
        if !ids.contains(id) || current.is_some_and(|set| !set.contains(id)) {
            mismatches.push(json!({"code":"canonical_id_not_found","path":format!("/data/models/{i}/id"),"canonical_id":id}));
        }
    }
    for (i, provider) in document.providers().iter().enumerate() {
        let name = provider["name"].as_str().context("provider name missing")?;
        for (j, offer) in provider["models"]
            .as_array()
            .context("models missing")?
            .iter()
            .enumerate()
        {
            let native = offer["provider_model_id"]
                .as_str()
                .context("native ID missing")?;
            let known = bindings.get(&(name, native)).copied();
            if let Some(id) = offer.get("canonical_id").and_then(Value::as_str) {
                references += 1;
                distinct.insert(id);
                if current.is_some_and(|set| !set.contains(id)) {
                    mismatches.push(json!({"code":"canonical_id_not_found","path":format!("/data/providers/{i}/models/{j}/canonical_id"),"provider":name,"provider_model_id":native}));
                }
                if known != Some(id) {
                    mismatches.push(json!({"code":"canonical_binding_conflict","path":format!("/data/providers/{i}/models/{j}/canonical_id"),"provider":name,"provider_model_id":native}));
                }
            } else {
                unbound += 1;
                if known.is_some_and(|id| current.is_some_and(|set| set.contains(id))) {
                    mismatches.push(json!({"code":"canonical_binding_conflict","path":format!("/data/providers/{i}/models/{j}/canonical_id"),"provider":name,"provider_model_id":native,"reason":"missing required current binding"}));
                }
            }
        }
    }
    Ok(
        json!({"outcome":if mismatches.is_empty(){"passed"}else{"invalid_ids"},"checked_references":references,"distinct_checked_ids":distinct.len(),"unbound_offers":unbound,"mismatches":mismatches,"historical_ids":ids.iter().filter(|id|current.is_some_and(|set|!set.contains(**id))).collect::<Vec<_>>()}),
    )
}
fn identity_ids(snapshot: &Value) -> Result<BTreeSet<String>> {
    Ok(bitrouter_registry::openrouter::identities(snapshot)?)
}
fn identity_request(manifest: &Value) -> Result<&Value> {
    let source = &manifest["identity_source"];
    if source["kind"] != "openrouter" {
        bail!("unsupported identity source");
    }
    let request = &source["request"];
    let mut url = reqwest::Url::parse(request["url"].as_str().context("identity URL missing")?)?;
    if let Some(query) = request.get("query") {
        for (k, v) in query.as_object().context("query must be object")? {
            url.query_pairs_mut()
                .append_pair(k, v.as_str().context("query string required")?);
        }
    }
    if url.origin().ascii_serialization() != "https://openrouter.ai"
        || url.path() != "/api/v1/models"
        || url.query_pairs().collect::<Vec<_>>() != vec![("output_modalities".into(), "all".into())]
    {
        bail!("identity source must request the fixed full OpenRouter model list");
    }
    Ok(request)
}

pub async fn sync(root: &Path, write: bool, report_path: Option<&Path>) -> Result<()> {
    fs::create_dir_all(root.join("target"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("target/registry-sync.lock"))?;
    lock.try_lock()
        .context("another registry sync is running")?;
    if !write && root.join(".registry-publication").exists() {
        bail!("unfinished publication; dry-run will not modify or recover it");
    }
    if write {
        recover_publication(root)?;
    }
    let original = Sources::load(root)?;
    original.document()?;
    let before = original.digest()?;
    let mut candidate = Sources::load(root)?;
    let transport = feed::Transport::new([])?;
    let now = Utc::now();
    let label = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut report = json!({"run":label,"mode":if write {"write"} else {"dry_run"},"feeds":[],"audit":{"outcome":"unavailable"}});
    let result=async {
        if original.files[&original.manifest]["as_of"].as_str().and_then(timestamp).is_none_or(|prior|now<prior) {bail!("publication as_of must not move backward");}
        let request=identity_request(&candidate.files[&candidate.manifest])?;
        let fetched=transport.identity(request,|env|std::env::var(env).ok()).await?;
        let snapshot=fetched.value;
        let ids=identity_ids(&snapshot)?;
        fs::create_dir_all(root.join("target"))?;
        atomic_write(&root.join("target/registry-identity-response.json"),&fetched.bytes)?;
        report["audit"]=json!({"outcome":"staged","source_url":"https://openrouter.ai/api/v1/models?output_modalities=all","fetched_at":label,"response_sha256":hex::encode(Sha256::digest(&fetched.bytes)),"run":label});
        let provider_paths=candidate.records("providers").map(|(p,_)|p.clone()).collect::<Vec<_>>();
        let mut failed=false;
        for path in provider_paths {
            let mut provider=candidate.files[&path].clone();
            let feeds=provider.pointer("/auto_sync/feeds").and_then(Value::as_array).cloned().unwrap_or_default();
            for feed in feeds {
                let id=feed["id"].as_str().context("feed ID missing")?;
                let staged=async {
                    let mut observation=transport.observe(&feed,|env|std::env::var(env).ok()).await?;
                    let stamp=bitrouter_registry::reconcile::next_stamp(&feed,label.clone(),Utc::now())?;
                    let pending=if feed["kind"]==bitrouter_registry::openrouter::KIND {vec![]} else {hold_unverified_bindings(&provider,&candidate.files[&candidate.identities],&ids,&mut observation)?};
                    let mut expanded=provider.clone();
                    inherit_interfaces(&mut expanded)?;
                    let (mut next,changes)=bitrouter_registry::reconcile::apply(&expanded,id,&observation,&stamp)?;
                    let mut ledger=candidate.files[&candidate.identities].clone();
                    if feed["kind"]==bitrouter_registry::openrouter::KIND {record_openrouter_bindings(&mut ledger,&provider,&observation,&stamp,&ids)?;}
                    materialize_bindings(&mut next,&ledger,&ids)?;
                    // Validate private provider data before making it visible to the next feed.
                    Document::from_value(json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":label,"data":{"models":[],"providers":[public_provider(&next)?]}}),&ReaderPolicy::default())?;
                    Ok::<_,anyhow::Error>((next,ledger,changes,stamp,observation.models.len(),pending))
                }.await;
                let item=match staged {
                    Ok((next,ledger,changes,stamp,count,pending))=>{candidate.files.insert(candidate.identities.clone(),ledger);provider=next;json!({"provider":provider["name"],"feed":id,"outcome":"updated","observation":stamp,"scope":feed["inventory"]["scope"],"complete":feed["inventory"]["complete"],"observed":count,"pending_bindings":pending,"changes":changes})},
                    Err(err)=>{failed=true; let diagnostic=err.downcast_ref::<bitrouter_registry::document::RegistryError>().map(|e|json!(e)).unwrap_or_else(||json!({"code":"registry.feed_failed","message":"Feed candidate failed validation; retain previous data"}));json!({"provider":provider["name"],"feed":id,"outcome":match diagnostic["code"].as_str() {Some("missing_secret")=>"missing_secret",Some("registry.sync_unsupported"|"registry.unsupported")=>"unsupported",Some("registry.incomplete"|"registry.sync_incomplete")=>"incomplete",_=>"failed"},"diagnostic":diagnostic})}
                };
                report["feeds"].as_array_mut().context("report feeds missing")?.push(item);
            }
            candidate.files.insert(path,provider);
        }
        let evaluation_time=Utc::now();
        candidate.files.get_mut(&candidate.manifest).context("manifest missing")?["as_of"]=json!(evaluation_time);
        let mut curated=candidate.clone();
        let discovery_result=(|| {discovery::apply(&mut curated,&snapshot,&ids,evaluation_time)?;reconcile_curated_availability(&mut curated)?;curated.document()?;Ok::<_,anyhow::Error>(())})();
        match discovery_result {
            Ok(())=>{candidate=curated;report["curation"]=json!({"outcome":"updated"});},
            Err(_)=>{failed=true;report["curation"]=json!({"outcome":"failed","code":"curation_discovery_failed","message":"Retained prior recommendation membership; independent feed proposals still require complete global admission"});}
        }
        reconcile_curated_availability(&mut candidate)?;
        let document=candidate.document()?;
        let checked=binding_report(&document,&candidate.files[&candidate.identities],Some(&ids))?;
        for (k,v) in checked.as_object().context("audit report object")? { report["audit"][k]=v.clone(); }
        if report["audit"]["outcome"]!="passed" {bail!("canonical identity audit rejected candidate; inspect structured mismatches");}
        report["audit"]["candidate_digest"]=json!(candidate.digest()?);
        report["audit"]["output_sha256"]=json!(hex::encode(Sha256::digest(rendered(&document)?)));
        if Sources::load(root)?.digest()? != before { bail!("source changed while sync was running; candidate not published"); }
        let outputs=build_outputs(root,&candidate)?;
        if write { candidate.publish(&original,&outputs)?; }
        report["candidate_ready"]=json!(true);
        if failed { bail!("one or more required feeds failed; independent validated changes were proposed"); }
        Ok(())
    }.await;
    if let Err(err) = &result {
        report["error"] = json!(format!("{err:#}"));
        if report["candidate_ready"] != true && report["audit"]["outcome"] != "invalid_ids" {
            report["audit"]["outcome"] = json!("unavailable");
        }
    }
    if let Some(path) = report_path {
        atomic_write(path, &serde_json::to_vec_pretty(&report)?)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    result
}
fn reconcile_curated_availability(sources: &mut Sources) -> Result<()> {
    let providers = sources
        .records("providers")
        .map(|(_, p)| public_provider(p))
        .collect::<Result<Vec<_>>>()?;
    let document = Document::from_value(
        json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":sources.files[&sources.manifest]["as_of"],"data":{"models":[],"providers":providers}}),
        &ReaderPolicy::default(),
    )?;
    let paths = sources
        .records("models")
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>();
    for path in paths {
        sources
            .files
            .get_mut(&path)
            .context("curated file")?
            .as_array_mut()
            .context("curated sequence")?
            .retain(|m| m["id"].as_str().is_some_and(|id| available(&document, id)));
    }
    Ok(())
}
fn public_provider(value: &Value) -> Result<Value> {
    let mut provider = value.clone();
    inherit_interfaces(&mut provider)?;
    let object = provider.as_object_mut().context("provider object")?;
    object.remove("auto_sync");
    object.remove("defaults");
    for model in object
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("provider models")?
    {
        model
            .as_object_mut()
            .context("model object")?
            .remove("sync");
    }
    Ok(provider)
}
fn inherit_interfaces(provider: &mut Value) -> Result<()> {
    let object = provider.as_object_mut().context("provider object")?;
    let interfaces = object
        .get("defaults")
        .map(|v| {
            let map = v.as_object().context("provider defaults must be object")?;
            if let Some(value) = map.get("interfaces")
                && !value.is_array()
            {
                bail!("defaults.interfaces must be an array");
            }
            Ok::<_, anyhow::Error>(map.get("interfaces").cloned())
        })
        .transpose()?
        .flatten();
    for model in object
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("provider models")?
    {
        let model = model.as_object_mut().context("model object")?;
        if !model.contains_key("interfaces")
            && let Some(defaults) = &interfaces
        {
            model.insert("interfaces".into(), defaults.clone());
        }
    }
    Ok(())
}
fn record_openrouter_bindings(
    ledger: &mut Value,
    provider: &Value,
    observation: &bitrouter_registry::mapping::Observation,
    stamp: &bitrouter_registry::reconcile::Stamp,
    ids: &BTreeSet<String>,
) -> Result<()> {
    if provider["name"] != "openrouter" || provider["api_base"] != "https://openrouter.ai/api/v1" {
        bail!("OpenRouter adapter is restricted to its declared own-provider endpoint");
    }
    let entries = ledger.as_array_mut().context("identity ledger")?;
    for observed in &observation.models {
        let native = observed.value["provider_model_id"]
            .as_str()
            .context("native ID")?;
        if observed.value["canonical_id"] != native || !ids.contains(native) {
            bail!(
                "canonical_binding_conflict: OpenRouter offer differs from independent full identity snapshot"
            );
        }
        let index = match entries.iter().position(|e| e["canonical_id"] == native) {
            Some(index) => index,
            None => {
                entries.push(json!({"canonical_id":native,"source_url":bitrouter_registry::openrouter::URL,"observed_at":stamp.observed_at}));
                entries.len() - 1
            }
        };
        let bindings = entries[index]
            .as_object_mut()
            .context("identity entry")?
            .entry("bindings")
            .or_insert(json!([]))
            .as_array_mut()
            .context("identity bindings")?;
        if !bindings
            .iter()
            .any(|b| b["provider"] == "openrouter" && b["provider_model_id"] == native)
        {
            bindings.push(json!({"provider":"openrouter","provider_model_id":native,"source_url":bitrouter_registry::openrouter::URL,"verified_at":stamp.observed_at}));
        }
    }
    entries.sort_by(|a, b| a["canonical_id"].as_str().cmp(&b["canonical_id"].as_str()));
    Ok(())
}
fn materialize_bindings(
    provider: &mut Value,
    ledger: &Value,
    ids: &BTreeSet<String>,
) -> Result<()> {
    let name = provider["name"]
        .as_str()
        .context("provider name")?
        .to_owned();
    let mut bindings = BTreeMap::new();
    for entry in ledger.as_array().context("ledger array")? {
        if !entry["canonical_id"]
            .as_str()
            .is_some_and(|id| ids.contains(id))
        {
            continue;
        }
        for binding in entry["bindings"].as_array().into_iter().flatten() {
            if binding["provider"] == name {
                bindings.insert(
                    binding["provider_model_id"].clone().to_string(),
                    entry["canonical_id"].clone(),
                );
            }
        }
    }
    for model in provider["models"].as_array_mut().context("model array")? {
        if let Some(id) = bindings.get(&model["provider_model_id"].to_string()) {
            if model.get("canonical_id").is_some_and(|old| old != id) {
                bail!("canonical_binding_conflict");
            }
            model["canonical_id"] = id.clone();
        } else if model.get("canonical_id").is_some() {
            // A fresh guessed mapping is not sufficient same-model evidence.
            // Existing source bindings fail the later global gate if delisted.
            // Unknown mappings must stay unbound until explicitly reviewed.
            bail!("canonical_binding_conflict: no reviewed current binding");
        }
    }
    Ok(())
}

// A generic list can propose an identity, but cannot establish same-model
// evidence. Keep newly discovered native inventory while reporting that work.
fn hold_unverified_bindings(
    provider: &Value,
    ledger: &Value,
    ids: &BTreeSet<String>,
    observation: &mut bitrouter_registry::mapping::Observation,
) -> Result<Vec<Value>> {
    let mut pending = Vec::new();
    for model in &mut observation.models {
        let Some(canonical) = model.value.get("canonical_id").cloned() else {
            continue;
        };
        let native = model.value["provider_model_id"].clone();
        let existing = provider["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["provider_model_id"] == native);
        if existing
            .and_then(|m| m.get("canonical_id"))
            .is_some_and(|v| v != &canonical)
        {
            bail!("canonical_binding_conflict: observed identity contradicts existing binding");
        }
        let verified = ledger
            .as_array()
            .context("identity ledger")?
            .iter()
            .any(|entry| {
                entry["canonical_id"] == canonical
                    && canonical.as_str().is_some_and(|id| ids.contains(id))
                    && entry["bindings"].as_array().is_some_and(|bindings| {
                        bindings.iter().any(|b| {
                            b["provider"] == provider["name"] && b["provider_model_id"] == native
                        })
                    })
            });
        if !verified && existing.and_then(|m| m.get("canonical_id")).is_none() {
            pending.push(json!({"provider_model_id":native,"proposed_canonical_id":canonical,"code":"canonical_binding_unverified"}));
            model
                .value
                .as_object_mut()
                .context("observation object")?
                .remove("canonical_id");
            model.observed.remove("/canonical_id");
        }
    }
    Ok(pending)
}

/// Audit final source/artifacts without running any provider feed or source edit.
pub async fn audit(root: &Path, report_path: Option<&Path>, previous: &[PathBuf]) -> Result<()> {
    let mut report = json!({"audit":{"outcome":"unavailable","mismatches":[],"code":"canonical_source_unavailable"}});
    let result = async {
        let sources = Sources::load(root)?;
        let document = sources.document()?;
        for path in previous {
            if document.as_of() < promoted_as_of(&fs::read(path)?)? {
                bail!("final candidate would roll back the promoted source");
            }
        }
        build(root, true)?;
        let transport = feed::Transport::new([])?;
        let response = transport
            .identity(
                identity_request(&sources.files[&sources.manifest])?,
                |env| std::env::var(env).ok(),
            )
            .await?;
        let ids = identity_ids(&response.value)?;
        atomic_write(
            &root.join("target/registry-identity-response.json"),
            &response.bytes,
        )?;
        let checked = binding_report(&document, &sources.files[&sources.identities], Some(&ids))?;
        report["audit"] = checked;
        report["audit"]["source_url"] = json!(bitrouter_registry::openrouter::URL);
        report["audit"]["fetched_at"] = json!(Utc::now());
        report["audit"]["response_sha256"] = json!(hex::encode(Sha256::digest(&response.bytes)));
        report["audit"]["candidate_digest"] = json!(sources.digest()?);
        report["audit"]["output_sha256"] = json!(hex::encode(Sha256::digest(rendered(&document)?)));
        report["audit"]["observation_id"] =
            json!(format!("audit-{}", Utc::now().timestamp_millis()));
        if Sources::load(root)?.tree_digest != sources.tree_digest {
            bail!("source changed during final audit");
        }
        build(root, true)?;
        if report["audit"]["outcome"] != "passed" {
            bail!("canonical identity audit rejected final candidate");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = &result {
        report["error"] = json!(format!("{error:#}"));
        if report["audit"]["outcome"] == "passed" {
            report["audit"]["outcome"] = json!("unavailable");
        }
    }
    if let Some(path) = report_path {
        atomic_write(path, &serde_json::to_vec_pretty(&report)?)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    result
}

// Trusted promotion history is only an ordering witness, never executable
// input to the current reader. Old beta structures need not remain supported.
fn promoted_as_of(bytes: &[u8]) -> Result<chrono::DateTime<Utc>> {
    let value = bitrouter_registry::document::parse_json(bytes)?;
    if integer(&value["version"]) != Some(1)
        || value["spec_revision"]
            .as_str()
            .is_none_or(|s| semver::Version::parse(s).is_err())
    {
        bail!("invalid promoted ordering witness");
    }
    value["as_of"]
        .as_str()
        .and_then(timestamp)
        .context("promoted UTC as_of missing")
}

#[cfg(test)]
mod tests;
