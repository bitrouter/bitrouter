//! Application validation and namespace policy for MCP skills.
//! Public wire records live in [`bitrouter_ai::protocol::mcp_skills`].
//! Content format parsing lives in [`crate::skills::format`].
//!
//! The gateway prefixes skill URIs with the member label without changing
//! frontmatter or content digests. Direct routes retain upstream URI schemes.
//! See [SEP-2640](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640)
//! and the [Agent Skills specification](https://agentskills.io/specification).

use bitrouter_ai::protocol::mcp_skills::{
    DYNAMIC_RESOURCES, GetSkillResult, ListSkillsResult, MAX_SKILL_RESOURCES,
    MAX_SKILL_TOTAL_BYTES, SKILL_SCHEME, SkillEntry, SkillResources, SkillsCacheScope,
    SkillsResultType,
};
#[cfg(test)]
use bitrouter_ai::protocol::mcp_skills::{
    GetSkillParams, RESOURCES_DIRECTORY_READ_METHOD, SKILLS_EXTENSION_ID, SKILLS_GET_METHOD,
    SKILLS_LIST_METHOD, SkillResource,
};

pub trait SkillEntryExt: Sized {
    fn validate_metadata(&self) -> Result<(), String>;
}

impl SkillEntryExt for SkillEntry {
    /// Validate metadata without fetching files or asserting trust in content.
    /// Direct routes retain legal URI schemes that aggregation cannot rewrite.
    fn validate_metadata(&self) -> Result<(), String> {
        let name = self
            .frontmatter
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or("invalid skill URI/name/description")?;
        let description = self
            .frontmatter
            .get("description")
            .and_then(|value| value.as_str());
        if !valid_agent_skill_name(name)
            || !description.is_some_and(|text| (1..=1024).contains(&text.chars().count()))
        {
            return Err("invalid skill URI/name/description".into());
        }
        skill_uri_root(&self.uri, name)?;
        let SkillResources::Enumerated(files) = &self.resources else {
            return Ok(());
        };
        if files.is_empty() || files.len() > MAX_SKILL_RESOURCES {
            return Err("skill manifest exceeds the supported file count or is empty".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut bytes = 0_u64;
        for file in files {
            if validate_skill_resource_uri(&file.uri).is_err()
                || !valid_sha256_digest(&file.digest)
                || !seen.insert(file.uri.as_str())
            {
                return Err("skill manifest has an invalid URI/digest or duplicate file".into());
            }
            bytes = bytes
                .checked_add(file.size)
                .ok_or("skill manifest byte count overflow")?;
            if bytes > MAX_SKILL_TOTAL_BYTES {
                return Err("skill manifest exceeds the supported byte budget".into());
            }
        }
        if !seen.contains(self.uri.as_str()) {
            return Err("skill manifest does not include its SKILL.md URI".into());
        }
        Ok(())
    }
}

pub trait ListSkillsResultExt: Sized {
    fn complete(skills: Vec<SkillEntry>, ttl_ms: u64, cache_scope: SkillsCacheScope) -> Self;
}

impl ListSkillsResultExt for ListSkillsResult {
    /// Build a conformant, synchronously complete listing.
    fn complete(skills: Vec<SkillEntry>, ttl_ms: u64, cache_scope: SkillsCacheScope) -> Self {
        Self {
            result_type: SkillsResultType::Complete,
            skills,
            ttl_ms,
            cache_scope,
        }
    }
}

pub trait GetSkillResultExt: Sized {
    fn complete(skill: SkillEntry, ttl_ms: u64, cache_scope: SkillsCacheScope) -> Self;
}

impl GetSkillResultExt for GetSkillResult {
    /// Build a conformant, synchronously complete skill lookup.
    fn complete(skill: SkillEntry, ttl_ms: u64, cache_scope: SkillsCacheScope) -> Self {
        Self {
            result_type: SkillsResultType::Complete,
            skill,
            ttl_ms,
            cache_scope,
        }
    }
}

/// Whether a configured server name is safe as one URI authority segment.
pub(crate) fn is_valid_gateway_label(label: &str) -> bool {
    !label.is_empty()
        && label.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'.' | b'_' | b'~')
        })
}

/// Prefix an upstream skill URI with `label`, moving it into the gateway's
/// namespace. Returns `None` for an unsafe label or a URI that is not
/// `skill://`-schemed; those entries are not aggregated (see the module docs).
pub fn namespace_uri(label: &str, uri: &str) -> Option<String> {
    if !is_valid_gateway_label(label) {
        return None;
    }
    let path = uri.strip_prefix(SKILL_SCHEME)?;
    Some(format!("{SKILL_SCHEME}{label}/{path}"))
}

/// Strip a gateway label back off a namespaced skill URI, recovering the URI
/// the upstream published. The exact inverse of [`namespace_uri`].
///
/// Returns `None` when `uri` is not `skill://`-schemed or does not start with
/// this label, which is what lets an unrewritten `skill://` URI fall through
/// to ordinary resolution instead of being mangled.
///
/// Returns a **complete URI**, scheme included — not the bare path. An earlier
/// version returned the path and left callers to re-attach `skill://`; one of
/// two call sites forgot, and the gateway forwarded a scheme-less path to an
/// upstream. Returning something callers can use directly removes the trap
/// rather than documenting it.
pub fn strip_label(label: &str, uri: &str) -> Option<String> {
    if !is_valid_gateway_label(label) {
        return None;
    }
    let path = uri
        .strip_prefix(SKILL_SCHEME)?
        .strip_prefix(label)?
        .strip_prefix('/')?;
    Some(format!("{SKILL_SCHEME}{path}"))
}

/// Shared Agent Skills name grammar: Unicode characters, not UTF-8 bytes.
pub fn valid_agent_skill_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name.chars().all(|c| {
            c == '-'
                || (c.is_alphanumeric()
                    && !c.is_uppercase()
                    && c.to_lowercase().eq(std::iter::once(c)))
        })
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// Validate absolute URI/IRI syntax without interpreting its scheme or
/// resolving an authority. Syntax alone does not identify or activate a skill.
pub fn validate_skill_resource_uri(uri: &str) -> Result<(), String> {
    skill_resource_path(uri).map(|_| ())
}

/// Read the original URI/IRI path independently of query and fragment identity.
pub fn skill_resource_path(uri: &str) -> Result<&str, String> {
    fluent_uri::Iri::parse(uri)
        .map(|parsed| parsed.path().as_str())
        .map_err(|_| "invalid absolute MCP skill resource URI".into())
}

/// Return the original directory URI, preserving its spelling and scheme.
/// The explicit file and parent-name constraints apply equally to rootless,
/// authority-based and absolute paths. No URL normalization is performed.
pub fn skill_uri_root<'a>(uri: &'a str, name: &str) -> Result<&'a str, String> {
    let parsed = fluent_uri::Iri::parse(uri).map_err(|_| "invalid MCP skill URI")?;
    let file_uri = uri.split(['?', '#']).next().ok_or("skill URI is missing")?;
    let root = file_uri
        .strip_suffix("/SKILL.md")
        .ok_or("skill URI must explicitly end in /SKILL.md")?;
    let path_root = parsed
        .path()
        .as_str()
        .strip_suffix("/SKILL.md")
        .ok_or("skill URI has no explicit SKILL.md path")?;
    let parent = if path_root.is_empty() {
        parsed
            .authority()
            .map(|authority| authority.as_str())
            .ok_or("skill parent directory is missing")?
    } else {
        path_root
            .rsplit('/')
            .next()
            .ok_or("skill parent directory is missing")?
    };
    if !valid_agent_skill_name(name) || decode_skill_uri_segment(parent)? != name {
        return Err("skill parent directory does not match frontmatter.name".into());
    }
    Ok(root)
}

/// Decode one URI segment without resolving an authority or changing origin.
pub fn decode_skill_uri_segment(segment: &str) -> Result<String, String> {
    let input = segment.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let encoded = segment
                .get(index + 1..index + 3)
                .ok_or("invalid percent-encoded skill URI")?;
            decoded.push(
                u8::from_str_radix(encoded, 16).map_err(|_| "invalid percent-encoded skill URI")?,
            );
            index += 3;
        } else {
            decoded.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "skill URI segment requires UTF-8".into())
}

fn has_dot_segment(uri: &str) -> bool {
    skill_resource_path(uri).map_or(true, |path| {
        path.split('/').any(|segment| {
            matches!(
                segment.to_ascii_lowercase().as_str(),
                "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
            )
        })
    })
}

/// Return the entry's skill-directory URI when the entry satisfies the
/// transport-level Agent Skills invariants a gateway can verify without
/// fetching content.
fn validated_skill_root(entry: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let uri = entry.get("uri")?.as_str()?;
    if !uri.starts_with(SKILL_SCHEME) || has_dot_segment(uri) {
        return None;
    }
    let frontmatter = entry.get("frontmatter")?.as_object()?;
    let name = frontmatter.get("name")?.as_str()?;
    let description = frontmatter.get("description")?.as_str()?;
    if !(1..=1024).contains(&description.chars().count()) {
        return None;
    }
    skill_uri_root(uri, name).ok().map(str::to_owned)
}

fn resource_belongs_to(root: &str, uri: &str) -> bool {
    if !uri.starts_with(SKILL_SCHEME) || has_dot_segment(uri) {
        return false;
    }
    uri.strip_prefix(root)
        .is_some_and(|suffix| suffix.starts_with('/') && suffix.len() > 1)
}

fn valid_sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Rewrite one `skills/list` / `skills/get` entry into the gateway namespace.
///
/// Returns `None` when the entry is not aggregatable — no `uri`, or a URI
/// under a scheme this gateway does not namespace. Callers surface those as
/// skipped rather than passing them through unrewritten, because an
/// unrewritten entry is exactly the collision the namespacing exists to
/// prevent.
///
/// Only `uri` fields are touched. `frontmatter` and every `digest` pass
/// through byte-identical, and unknown fields — including ones added by
/// future revisions of the Agent Skills spec — are preserved, which is why
/// this operates on `serde_json::Value` rather than a typed entry.
pub fn namespace_entry(label: &str, entry: &serde_json::Value) -> Option<serde_json::Value> {
    let mut entry = entry.clone();
    let object = entry.as_object_mut()?;
    let root = validated_skill_root(object)?;
    let uri = object.get("uri")?.as_str()?.to_string();
    let namespaced = namespace_uri(label, &uri)?;
    object.insert("uri".to_string(), namespaced.into());

    // `resources` is the integrity commitment a host's approval binds to, so
    // every entry in it has to move with the skill — and the field is REQUIRED,
    // so an entry without it is one a conforming host must refuse. Republishing
    // it under our own namespace would be putting our name on an invalid entry.
    let resources_value = object.get_mut("resources")?;
    if resources_value.as_str() == Some(DYNAMIC_RESOURCES) {
        // A dynamic skill carries no URIs to rewrite, and nothing to count
        // against the limits.
        return Some(entry);
    }
    let resources = resources_value.as_array_mut()?;
    if resources.is_empty() || resources.len() > MAX_SKILL_RESOURCES {
        return None;
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut includes_skill_md = false;
    let mut total_bytes: u64 = 0;
    for resource in resources.iter_mut() {
        let resource = resource.as_object_mut()?;
        let resource_uri = resource.get("uri")?.as_str()?.to_string();
        let digest = resource.get("digest")?.as_str()?;
        // `size` is required on every entry: it is what lets a host budget the
        // skill before fetching anything, and a length mismatch on a later read
        // is a verification failure in its own right.
        let size = resource.get("size")?.as_u64()?;
        if !valid_sha256_digest(digest)
            || !seen.insert(resource_uri.clone())
            || !resource_belongs_to(&root, &resource_uri)
        {
            return None;
        }
        total_bytes = total_bytes.checked_add(size)?;
        if total_bytes > MAX_SKILL_TOTAL_BYTES {
            return None;
        }
        includes_skill_md |= resource_uri == uri;
        let namespaced = namespace_uri(label, &resource_uri)?;
        resource.insert("uri".to_string(), namespaced.into());
    }
    if !includes_skill_md {
        return None;
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frontmatter(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match json {
            serde_json::Value::Object(map) => map,
            other => panic!("frontmatter must be an object, got {other}"),
        }
    }

    fn entry(uri: &str, name: &str) -> SkillEntry {
        SkillEntry {
            uri: uri.into(),
            frontmatter: frontmatter(serde_json::json!({"name": name, "description": "d"})),
            resources: SkillResources::Enumerated(vec![SkillResource {
                uri: uri.into(),
                digest: DIGEST.into(),
                size: 12,
            }]),
            extra: serde_json::Map::new(),
        }
    }

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// The SEP's own `skills/list` example must round-trip byte-identically.
    #[test]
    fn sep_example_round_trips() {
        let wire = serde_json::json!({
            "resultType": "complete",
            "skills": [
                {
                    "uri": "skill://git-workflow/SKILL.md",
                    "frontmatter": {
                        "name": "git-workflow",
                        "description": "Follow this team's Git conventions"
                    },
                    "resources": [
                        {
                            "uri": "skill://git-workflow/SKILL.md",
                            "digest": "sha256:a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2",
                            "size": 2314
                        }
                    ]
                },
                {
                    "uri": "skill://acme/billing/refunds/SKILL.md",
                    "frontmatter": {
                        "name": "refunds",
                        "description": "Process customer refund requests",
                        "license": "Apache-2.0"
                    },
                    "resources": [
                        {
                            "uri": "skill://acme/billing/refunds/SKILL.md",
                            "digest": "sha256:b2c3d4e5b2c3d4e5b2c3d4e5b2c3d4e5b2c3d4e5b2c3d4e5b2c3d4e5b2c3d4e5",
                            "size": 3871
                        }
                    ]
                }
            ],
            "ttlMs": 300000,
            "cacheScope": "public"
        });
        let parsed: ListSkillsResult = serde_json::from_value(wire.clone()).expect("parses");
        assert_eq!(serde_json::to_value(&parsed).expect("serializes"), wire);
    }

    /// Frontmatter is verbatim: a field this crate has never heard of must
    /// survive a round trip, because the Agent Skills spec evolves
    /// independently of the transport binding.
    #[test]
    fn unknown_frontmatter_fields_survive() {
        let mut e = entry("skill://x/SKILL.md", "x");
        e.frontmatter.insert(
            "some-future-field".into(),
            serde_json::json!({"nested": [1, 2, 3]}),
        );
        let round: SkillEntry =
            serde_json::from_value(serde_json::to_value(&e).expect("ser")).expect("de");
        assert_eq!(round, e);
        assert_eq!(
            round.frontmatter["some-future-field"]["nested"][2],
            serde_json::json!(3)
        );
    }

    /// The same guarantee one level up. SEP-2640 is a draft, so an entry may
    /// carry top-level fields we do not model; routing one through this type
    /// must not quietly drop them. This is what lets the gateway rewrite a
    /// typed entry without degrading what the upstream published.
    #[test]
    fn unknown_top_level_entry_fields_survive() {
        let wire = serde_json::json!({
            "uri": "skill://x/SKILL.md",
            "frontmatter": {"name": "x", "description": "d"},
            "resources": [{"uri": "skill://x/SKILL.md", "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 12}],
            "someFutureField": {"attestation": "signed"},
            "anotherOne": 7
        });
        let parsed: SkillEntry = serde_json::from_value(wire.clone()).expect("parses");
        assert_eq!(
            parsed.extra["someFutureField"]["attestation"],
            serde_json::json!("signed")
        );
        assert_eq!(
            serde_json::to_value(&parsed).expect("serializes"),
            wire,
            "an unmodelled field must survive the trip untouched"
        );
    }

    /// The accepted SEP's two forms, and only those two. `"dynamic"` is a
    /// *string marker*, not an omitted key — the change exists "so that a host
    /// can tell a deliberately unverifiable skill from a malformed entry".
    #[test]
    fn resources_is_an_array_or_the_dynamic_marker() {
        let dynamic = SkillEntry {
            resources: SkillResources::Dynamic,
            ..entry("skill://x/SKILL.md", "x")
        };
        let wire = serde_json::to_value(&dynamic).expect("ser");
        assert_eq!(wire["resources"], serde_json::json!("dynamic"));
        let back: SkillEntry = serde_json::from_value(wire).expect("de");
        assert_eq!(back.resources, SkillResources::Dynamic);
        assert_eq!(back.resources.entries(), None);

        let enumerated = entry("skill://x/SKILL.md", "x");
        let wire = serde_json::to_value(&enumerated).expect("ser");
        assert_eq!(wire["resources"][0]["size"], serde_json::json!(12));
        let back: SkillEntry = serde_json::from_value(wire).expect("de");
        assert_eq!(back.resources.entries().map(<[_]>::len), Some(1));
    }

    /// "An entry with no `resources` at all, or with any value other than an
    /// array or `\"dynamic\"`, is invalid, and hosts MUST NOT load it." A type
    /// that parsed those would hand a caller an entry a conforming host refuses.
    #[test]
    fn an_entry_without_a_valid_resources_form_does_not_parse() {
        for wire in [
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"}
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": "generated"
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": null
            }),
            // `size` is required on every entry.
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "skill://x/SKILL.md", "digest": DIGEST}]
            }),
        ] {
            assert!(
                serde_json::from_value::<SkillEntry>(wire.clone()).is_err(),
                "an invalid entry parsed: {wire}"
            );
        }
    }

    /// The two limits are fixed by the accepted SEP, not chosen by us.
    #[test]
    fn per_skill_limits_match_the_sep() {
        assert_eq!(MAX_SKILL_RESOURCES, 512);
        assert_eq!(MAX_SKILL_TOTAL_BYTES, 16_777_216);
        assert_eq!(DYNAMIC_RESOURCES, "dynamic");
    }

    #[test]
    fn get_params_and_result_use_the_sep_field_names() {
        let params: GetSkillParams =
            serde_json::from_value(serde_json::json!({"uri": "skill://pdf/SKILL.md"}))
                .expect("parses");
        assert_eq!(params.uri, "skill://pdf/SKILL.md");

        let result = GetSkillResult::complete(
            entry("skill://pdf/SKILL.md", "pdf"),
            0,
            SkillsCacheScope::Public,
        );
        let wire = serde_json::to_value(&result).expect("ser");
        assert_eq!(wire["resultType"], "complete");
        assert_eq!(wire["skill"]["uri"], "skill://pdf/SKILL.md");
        assert_eq!(wire["ttlMs"], 0);
        assert_eq!(wire["cacheScope"], "public");
    }

    /// The method names the relay allowlist and the origin server's dispatch
    /// both spell. One declaration, so the two cannot drift.
    #[test]
    fn method_names_match_the_sep() {
        assert_eq!(SKILLS_LIST_METHOD, "skills/list");
        assert_eq!(SKILLS_GET_METHOD, "skills/get");
        assert_eq!(RESOURCES_DIRECTORY_READ_METHOD, "resources/directory/read");
        assert_eq!(SKILLS_EXTENSION_ID, "io.modelcontextprotocol/skills");
    }

    #[test]
    fn unicode_names_use_character_limits_and_keep_encoded_uri_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        for name in ["审阅", "révision-٢", &"é".repeat(64)] {
            assert!(valid_agent_skill_name(name));
        }
        for name in ["É", "ǅ", "𝐀", "review--check", "审阅-😀", &"é".repeat(65)] {
            assert!(!valid_agent_skill_name(name));
        }
        let uri = "skill://%E5%AE%A1%E9%98%85/SKILL.md";
        let skill: SkillEntry = serde_json::from_value(serde_json::json!({
            "uri":uri,"frontmatter":{"name":"审阅","description":"unicode workflow","custom":"unchanged"},
            "resources":[{"uri":uri,"size":1,"digest":format!("sha256:{}", "a".repeat(64))}]
        }))?;
        skill.validate_metadata()?;
        let namespaced = namespace_entry("source", &serde_json::to_value(&skill)?)
            .ok_or("unicode entry was dropped")?;
        assert_eq!(
            namespaced["uri"],
            "skill://source/%E5%AE%A1%E9%98%85/SKILL.md"
        );
        assert_eq!(
            namespaced["frontmatter"],
            serde_json::to_value(&skill.frontmatter)?
        );
        assert_eq!(
            strip_label(
                "source",
                namespaced["uri"].as_str().ok_or("namespaced URI")?
            )
            .as_deref(),
            Some(uri)
        );
        assert!(decode_skill_uri_segment("%ZZ").is_err());
        assert!(decode_skill_uri_segment("%C3").is_err());
        Ok(())
    }

    #[test]
    fn namespacing_round_trips() {
        let namespaced = namespace_uri("acme", "skill://refunds/SKILL.md").expect("skill uri");
        assert_eq!(namespaced, "skill://acme/refunds/SKILL.md");
        assert_eq!(
            strip_label("acme", &namespaced).as_deref(),
            Some("skill://refunds/SKILL.md"),
            "the inverse recovers exactly what the upstream published, scheme included"
        );
    }

    #[test]
    fn unsafe_gateway_labels_are_rejected() {
        for label in [
            "",
            "Upper",
            "A",
            "a/b",
            "what?",
            "frag#",
            "has space",
            "percent%2f",
        ] {
            assert!(
                namespace_uri(label, "skill://x/SKILL.md").is_none(),
                "unsafe label was accepted: {label:?}"
            );
        }
    }

    #[test]
    fn malformed_skill_entries_are_not_namespaced() {
        let malformed = [
            serde_json::json!({
                "uri": "skill://x/not-skill.md",
                "frontmatter": {"name": "x", "description": "d"}
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "other", "description": "d"}
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "https://evil.test/payload", "digest": DIGEST, "size": 1}]
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "skill://other/file", "digest": DIGEST, "size": 1}]
            }),
        ];

        for entry in malformed {
            assert!(
                namespace_entry("upstream", &entry).is_none(),
                "malformed entry was published: {entry}"
            );
        }
    }

    #[test]
    fn resource_manifest_must_be_complete_unique_and_digest_bound() {
        let malformed = [
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": []
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "skill://x/other.md", "digest": DIGEST, "size": 1}]
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [
                    {"uri": "skill://x/SKILL.md", "digest": DIGEST, "size": 1},
                    {"uri": "skill://x/SKILL.md", "digest": DIGEST, "size": 1}
                ]
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "skill://x/SKILL.md", "size": 1}]
            }),
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "skill://x/SKILL.md", "digest": "sha256:AA", "size": 1}]
            }),
            // `size` missing: the accepted SEP requires it on every entry, and
            // without it a host cannot apply the limits before fetching.
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [{"uri": "skill://x/SKILL.md", "digest": DIGEST}]
            }),
            // 16 MiB + 1 byte, summed over `size`.
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": [
                    {"uri": "skill://x/SKILL.md", "digest": DIGEST, "size": MAX_SKILL_TOTAL_BYTES},
                    {"uri": "skill://x/big.bin", "digest": DIGEST, "size": 1}
                ]
            }),
            // 513 entries.
            serde_json::json!({
                "uri": "skill://x/SKILL.md",
                "frontmatter": {"name": "x", "description": "d"},
                "resources": (0..=MAX_SKILL_RESOURCES)
                    .map(|i| serde_json::json!({
                        "uri": format!("skill://x/f{i}.md"),
                        "digest": DIGEST,
                        "size": 1
                    }))
                    .collect::<Vec<_>>()
            }),
        ];

        for entry in malformed {
            assert!(
                namespace_entry("upstream", &entry).is_none(),
                "invalid integrity manifest was published: {entry}"
            );
        }
    }

    #[test]
    fn namespacing_preserves_the_name_invariant() {
        // A nested upstream path keeps its final segment, which is the name.
        let namespaced =
            namespace_uri("srv", "skill://acme/billing/refunds/SKILL.md").expect("skill uri");
        assert_eq!(namespaced, "skill://srv/acme/billing/refunds/SKILL.md");
        let final_segment = namespaced
            .strip_suffix("/SKILL.md")
            .and_then(|p| p.rsplit('/').next());
        assert_eq!(final_segment, Some("refunds"));
    }

    #[test]
    fn other_schemes_are_not_namespaced() {
        assert_eq!(
            namespace_uri("srv", "github://owner/repo/skills/x/SKILL.md"),
            None
        );
        assert_eq!(namespace_uri("srv", "file:///etc/passwd"), None);
    }

    #[test]
    fn strip_label_ignores_other_labels_and_schemes() {
        assert_eq!(strip_label("a", "skill://b/refunds/SKILL.md"), None);
        assert_eq!(strip_label("a", "https://a/refunds"), None);
        // A label that is a prefix of another must not partially match.
        assert_eq!(strip_label("a", "skill://ab/refunds/SKILL.md"), None);
    }

    #[test]
    fn entry_rewrites_every_uri_and_touches_nothing_else() {
        let entry = serde_json::json!({
            "uri": "skill://refunds/SKILL.md",
            "frontmatter": {
                "name": "refunds",
                "description": "Process refunds",
                "some-future-field": {"nested": true}
            },
            "resources": [
                {"uri": "skill://refunds/SKILL.md", "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 3871},
                {"uri": "skill://refunds/examples/email.md", "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "size": 962}
            ]
        });
        let out = namespace_entry("acme", &entry).expect("aggregatable");

        assert_eq!(out["uri"], "skill://acme/refunds/SKILL.md");
        // `size` rides along untouched: it is a property of the bytes, like the
        // digest, and a host budgets the skill from it.
        assert_eq!(out["resources"][0]["size"], serde_json::json!(3871));
        assert_eq!(out["resources"][1]["size"], serde_json::json!(962));
        assert_eq!(out["resources"][0]["uri"], "skill://acme/refunds/SKILL.md");
        assert_eq!(
            out["resources"][1]["uri"],
            "skill://acme/refunds/examples/email.md"
        );
        // Digests are over content bytes, so rewriting a URI must not disturb
        // them — a changed digest would revoke a host's content-bound approval.
        assert_eq!(
            out["resources"][0]["digest"],
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            out["resources"][1]["digest"],
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        // Frontmatter passes through byte-identical, unknown fields included.
        assert_eq!(out["frontmatter"], entry["frontmatter"]);
    }

    #[test]
    fn entry_under_another_scheme_is_skipped() {
        let entry = serde_json::json!({
            "uri": "github://owner/repo/skills/x/SKILL.md",
            "frontmatter": {"name": "x", "description": "d"}
        });
        assert_eq!(namespace_entry("srv", &entry), None);
    }

    #[test]
    fn entry_without_a_uri_is_skipped() {
        let entry = serde_json::json!({ "frontmatter": {"name": "x"} });
        assert_eq!(namespace_entry("srv", &entry), None);
    }

    #[test]
    fn generic_uri_paths_preserve_identity_and_require_the_exact_skill_parent() -> Result<(), String>
    {
        for uri in [
            "urn:review/SKILL.md",
            "urn:acme/review/SKILL.md",
            "urn:/review/SKILL.md",
            "https:review/SKILL.md",
            "https://Example.invalid/review/SKILL.md",
            "skill://review/SKILL.md",
        ] {
            validate_skill_resource_uri(uri)?;
            assert_eq!(
                skill_uri_root(uri, "review")?,
                uri.strip_suffix("/SKILL.md").ok_or("suffix")?
            );
            entry(uri, "review").validate_metadata()?;
        }
        for uri in [
            "review/SKILL.md",
            "urn:SKILL.md",
            "urn:other/SKILL.md",
            "urn:review/OTHER.md",
            "skill://review/OTHER.md?next=/SKILL.md",
            "skill://review/OTHER.md#SKILL.md",
        ] {
            assert!(skill_uri_root(uri, "review").is_err(), "accepted {uri}");
            assert!(
                entry(uri, "review").validate_metadata().is_err(),
                "accepted entry {uri}"
            );
        }
        Ok(())
    }

    #[test]
    fn query_and_fragment_identity_survive_directory_mapping_and_namespacing() -> Result<(), String>
    {
        let uri = "skill://review/SKILL.md?revision=1#instructions";
        let support = "skill://review/references/check.md?version=2&tag=../other#section";
        assert_eq!(skill_uri_root(uri, "review")?, "skill://review");
        assert_eq!(skill_resource_path(uri)?, "/SKILL.md");
        let mut value =
            serde_json::to_value(entry(uri, "review")).map_err(|error| error.to_string())?;
        value["resources"]
            .as_array_mut()
            .ok_or("resources")?
            .push(serde_json::json!({"uri":support,"digest":DIGEST,"size":14}));
        serde_json::from_value::<SkillEntry>(value.clone())
            .map_err(|error| error.to_string())?
            .validate_metadata()?;
        let mapped = namespace_entry("source", &value).ok_or("namespace mapping")?;
        let root = "skill://source/review/SKILL.md?revision=1#instructions";
        let file = "skill://source/review/references/check.md?version=2&tag=../other#section";
        assert_eq!(mapped["uri"], root);
        assert_eq!(mapped["resources"][1]["uri"], file);
        assert_eq!(strip_label("source", root).as_deref(), Some(uri));
        assert_eq!(strip_label("source", file).as_deref(), Some(support));
        Ok(())
    }

    /// A dynamic skill carries the marker through unchanged — there are no URIs
    /// in it to rewrite — while an entry that simply omits `resources` is one a
    /// conforming host must refuse, so the gateway will not republish it under
    /// its own name.
    #[test]
    fn the_dynamic_marker_survives_and_an_omitted_key_does_not() {
        let dynamic = serde_json::json!({
            "uri": "skill://generated/SKILL.md",
            "frontmatter": {"name": "generated", "description": "d"},
            "resources": "dynamic"
        });
        let out = namespace_entry("srv", &dynamic).expect("aggregatable");
        assert_eq!(out["uri"], "skill://srv/generated/SKILL.md");
        assert_eq!(out["resources"], serde_json::json!("dynamic"));

        let omitted = serde_json::json!({
            "uri": "skill://generated/SKILL.md",
            "frontmatter": {"name": "generated", "description": "d"}
        });
        assert_eq!(namespace_entry("srv", &omitted), None);
    }
}
