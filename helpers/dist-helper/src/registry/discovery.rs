//! Curated selection only; never writes provider inventory or bindings.
use super::*;
use regex::Regex;

struct Rule {
    key: String,
    pattern: Regex,
    excluded: Vec<Regex>,
    retain: usize,
}
fn number(value: &Value, key: &str, default: u64) -> Result<u64> {
    match value.get(key) {
        None => Ok(default),
        Some(value) => {
            integer(value).with_context(|| format!("discovery {key} must be a nonnegative integer"))
        }
    }
}
fn rules(policy: &Value) -> Result<(Vec<Rule>, u64)> {
    let default = number(policy, "retain_versions", 2)?;
    if default == 0 || default > 1000 {
        bail!("invalid default retention window");
    }
    let days = number(policy, "deprecation_days", 30)?;
    chrono::Duration::try_days(days.try_into()?).context("invalid discovery grace window")?;
    let mut rules = Vec::new();
    let mut keys = BTreeSet::new();
    for value in policy["families"]
        .as_array()
        .context("discovery families must be an array")?
    {
        let key = value["key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("family key")?;
        if !keys.insert(key) {
            bail!("duplicate discovery family key");
        }
        let retain = number(value, "retain_versions", default)?;
        if retain == 0 || retain > 1000 {
            bail!("invalid retention window");
        }
        let mut excluded = Vec::new();
        if let Some(patterns) = value.get("exclude_variants") {
            for pattern in patterns
                .as_array()
                .context("exclude_variants must be an array")?
            {
                excluded.push(Regex::new(
                    pattern.as_str().context("variant pattern string")?,
                )?);
            }
        }
        let pattern = Regex::new(value["pattern"].as_str().context("family pattern")?)?;
        if !pattern.capture_names().any(|name| name == Some("version")) {
            bail!("family pattern needs named numeric version capture");
        }
        rules.push(Rule {
            key: key.into(),
            pattern,
            excluded,
            retain: retain as usize,
        });
    }
    Ok((rules, days))
}
pub(super) fn validate(policy: &Value) -> Result<()> {
    rules(policy)?;
    let mut ids = BTreeSet::new();
    for record in policy["retired_models"]
        .as_array()
        .context("retired_models must be evidence objects")?
    {
        let id = record["id"]
            .as_str()
            .filter(|s| canonical_id(s))
            .context("tombstone ID")?;
        if !ids.insert(id)
            || !https(&record["source_url"])
            || record["recorded_at"].as_str().and_then(timestamp).is_none()
            || record["reason"].as_str().is_none_or(str::is_empty)
        {
            bail!("invalid curation tombstone evidence");
        }
    }
    Ok(())
}
fn classify(id: &str, rules: &[Rule]) -> Result<Option<(String, Vec<u64>, usize)>> {
    let mut result = None;
    for rule in rules {
        let Some(captures) = rule.pattern.captures(id) else {
            continue;
        };
        if rule.excluded.iter().any(|r| r.is_match(id)) {
            continue;
        }
        let version = captures
            .name("version")
            .context("family pattern needs named version")?
            .as_str()
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        let key = if let Some(series) = captures.name("series") {
            rule.key.replace("{series}", series.as_str())
        } else {
            rule.key.clone()
        };
        if result.is_some() {
            bail!("overlapping discovery families");
        }
        result = Some((key, version, rule.retain));
    }
    Ok(result)
}
fn compare(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    (0..a.len().max(b.len()))
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .cmp(&b.get(i).copied().unwrap_or(0))
        })
        .find(|o| !o.is_eq())
        .unwrap_or(std::cmp::Ordering::Equal)
}

pub(super) fn apply(
    sources: &mut Sources,
    snapshot: &Value,
    ids: &BTreeSet<String>,
    now: chrono::DateTime<Utc>,
) -> Result<()> {
    let policy_path = sources
        .manifest
        .parent()
        .context("registry parent")?
        .join("model-discovery.yaml");
    let policy = &sources.files[&policy_path];
    let (rules, days) = rules(policy)?;
    let tombstones = policy["retired_models"]
        .as_array()
        .context("retired_models must be evidence objects")?;
    let mut retired = BTreeSet::new();
    for record in tombstones {
        if !https(&record["source_url"])
            || record["recorded_at"].as_str().and_then(timestamp).is_none()
            || record["reason"].as_str().is_none_or(str::is_empty)
        {
            bail!("invalid curation tombstone evidence");
        }
        retired.insert(record["id"].as_str().context("tombstone ID")?.to_owned());
    }
    // Eligibility uses the staged provider inventory, independently of curation.
    let providers = sources
        .records("providers")
        .map(|(_, p)| public_provider(p))
        .collect::<Result<Vec<_>>>()?;
    let eligibility = Document::from_value(
        json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":now.to_rfc3339_opts(chrono::SecondsFormat::AutoSi,true),"data":{"models":[],"providers":providers}}),
        &ReaderPolicy::default(),
    )?;
    let mut families = BTreeMap::<String, (usize, Vec<Vec<u64>>)>::new();
    let mut eligible = BTreeMap::new();
    for row in snapshot["data"]
        .as_array()
        .context("identity data missing")?
    {
        let id = row["id"].as_str().context("model ID")?;
        if retired.contains(id) || !available(&eligibility, id) {
            continue;
        }
        if let Some((family, version, retain)) = classify(id, &rules)? {
            let (prior, versions) = families
                .entry(family.clone())
                .or_insert((retain, Vec::new()));
            if *prior != retain {
                bail!("conflicting retention rules");
            }
            if !versions.iter().any(|v| compare(v, &version).is_eq()) {
                versions.push(version.clone());
            }
            eligible.insert(id.to_owned(), (family, version, row));
        }
    }
    for (retain, versions) in families.values_mut() {
        versions.sort_by(|a, b| compare(b, a));
        versions.truncate(*retain);
    }
    let retained = eligible
        .iter()
        .filter(|(_, (family, version, _))| {
            families
                .get(family)
                .is_some_and(|(_, versions)| versions.iter().any(|v| compare(v, version).is_eq()))
        })
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let mut existing = BTreeSet::new();
    let paths = sources
        .records("models")
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>();
    let expiry = now
        .checked_add_signed(chrono::Duration::try_days(days.try_into()?).context("grace overflow")?)
        .context("grace timestamp overflow")?;
    for path in paths {
        let mut records = sources.files[&path]
            .as_array()
            .context("curated sequence")?
            .clone();
        records.retain(|record| {
            record["id"].as_str().is_some_and(|id| {
                ids.contains(id) && !retired.contains(id) && available(&eligibility, id)
            })
        });
        for record in &mut records {
            let id = record["id"].as_str().context("curated ID")?.to_owned();
            existing.insert(id.clone());
            if retained.contains(&id) {
                record["curation"]
                    .as_object_mut()
                    .context("curation object")?
                    .remove("expires_at");
            } else if classify(&id, &rules)?.is_some()
                && record["curation"].get("expires_at").is_none()
            {
                record["curation"]["expires_at"] =
                    json!(expiry.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
            }
        }
        records.retain(|r| {
            r.pointer("/curation/expires_at")
                .and_then(Value::as_str)
                .and_then(timestamp)
                .is_none_or(|t| t > now)
        });
        sources.files.insert(path, json!(records));
    }
    // A new curated record also needs reviewed identity evidence. Newly observed
    // IDs with no binding never become recommended just because a regex matches.
    let known = sources.files[&sources.identities]
        .as_array()
        .context("identity sequence")?
        .iter()
        .filter_map(|e| e["canonical_id"].as_str())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    for id in retained.difference(&existing) {
        if !known.contains(id) {
            continue;
        }
        let (_, _, row) = &eligible[id];
        let namespace = id.split_once('/').context("canonical namespace")?.0;
        let path = sources
            .manifest
            .parent()
            .context("registry parent")?
            .join(format!("models/{namespace}.yaml"));
        let mut record = json!({"id":id,"curation":{"reason":"Eligible release in approved family retention window","source_url":"https://openrouter.ai/api/v1/models?output_modalities=all","checked_at":now.to_rfc3339_opts(chrono::SecondsFormat::AutoSi,true)}});
        for key in ["name", "description"] {
            if let Some(value) = row.get(key) {
                record[key] = value.clone();
            }
        }
        for key in ["input_modalities", "output_modalities"] {
            if let Some(value) = row["architecture"].get(key) {
                record[key] = value.clone();
            }
        }
        if let Some(value) = row.get("context_length") {
            record["context_window"] = value.clone();
        }
        sources
            .files
            .entry(path)
            .or_insert(json!([]))
            .as_array_mut()
            .context("curated sequence")?
            .push(record);
    }
    Ok(())
}
