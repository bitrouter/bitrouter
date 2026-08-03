//! The SEP-2640 skills protocol surface: wire shapes, method names, and the
//! URI namespacing the gateway applies when aggregating skills across upstream
//! MCP servers.
//!
//! [SEP-2640] (`io.modelcontextprotocol/skills`) serves
//! [Agent Skills](https://agentskills.io/) over the existing MCP Resources
//! primitive. Skills are deliberately **not** a new primitive — SEP-2076
//! proposed that and lost, because name-addressed blobs destroy the directory
//! model supporting files depend on — so this module lives inside [`crate::mcp`]
//! rather than beside it.
//!
//! This module owns only the *transport* shapes. The skill format itself —
//! frontmatter fields, naming rules, progressive disclosure — is delegated
//! entirely to the [Agent Skills specification], exactly as the SEP delegates
//! it.
//!
//! ## Why these types are here and not in the origin-server crate
//!
//! Both halves of the gateway speak these shapes, and so does anything that
//! wants to *reason* about skills in flight. The [`super::PreRequestHook`] /
//! [`super::RouteHook`] / [`super::ExecutionHook`] traits see raw JSON
//! (`McpRequest::params`, `McpResponse::result`), so a hook that wants to apply
//! policy to a skill — refuse one whose frontmatter declares `allowed-tools`,
//! or record which skills entered which agent's context — would otherwise have
//! to re-derive these shapes downstream. Keeping them beside the hook traits is
//! what stops that drift.
//!
//! The *port* a server implements to serve skills is a different concern and
//! lives with its siblings in `bitrouter-mcp::capabilities::skill_catalog`; so
//! does everything about fetching and installing skills, which stays in
//! `bitrouter-skills`.
//!
//! [SEP-2640]: https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640
//! [Agent Skills specification]: https://agentskills.io/specification
//!
//! ## Namespacing
//!
//! The gateway merges N upstream skill namespaces into one, and two upstreams
//! can legitimately publish the same URI — SEP-2640 calls that out as an
//! impersonation surface:
//!
//! > a malicious server can publish a skill under the name of a popular one …
//! > Hosts MUST resolve skill names within a per-origin namespace
//!
//! The trick the gateway uses for tools — prepending `{server}__` to the name
//! — is unavailable here. SEP-2640 requires the final `<skill-path>` segment
//! to equal `frontmatter.name`, and requires the entry's `frontmatter` to match
//! the fetched `SKILL.md` field-by-field, so prefixing the *name* would break
//! both invariants. Namespacing therefore happens in the **URI prefix**:
//!
//! ```text
//! upstream   skill://refunds/SKILL.md          (member "acme")
//! gateway    skill://acme/refunds/SKILL.md
//! ```
//!
//! This is legal without qualification — preceding segments are, in the SEP's
//! words, "a server-chosen organizational prefix" — and it preserves what
//! matters:
//!
//! - the final segment is still `refunds`, so the name/URI invariant holds;
//! - `frontmatter` is never touched;
//! - digests are over content *bytes*, not URIs, so rewriting cannot
//!   invalidate them.
//!
//! Rewriting is a pure function of `(label, uri)` in both directions, so the
//! gateway stays stateless per request — no session table, no cached mapping.
//! That statelessness is exactly why only `skill://` is aggregated: reversing a
//! rewrite of an arbitrary scheme would mean remembering the original scheme
//! per skill, which is state. Skills under another scheme stay reachable
//! through the member's direct route, where there is one member and so no
//! collision to resolve.

use serde::{Deserialize, Serialize};

/// The conventional URI scheme for Agent Skills served over MCP.
///
/// An *addressing* scheme, not a fetch scheme: SEP-2640 states that clients
/// "MUST NOT attempt DNS or network resolution" of the authority component. A
/// `skill://` URI is meaningful only relative to the server that published it,
/// which is why the gateway must namespace one before merging it with another.
pub const SKILL_SCHEME: &str = "skill://";

/// The SEP-2133 extension identifier declared in `initialize`.
pub const SKILLS_EXTENSION_ID: &str = "io.modelcontextprotocol/skills";

/// The `skills/list` JSON-RPC method.
pub const SKILLS_LIST_METHOD: &str = "skills/list";

/// The `skills/get` JSON-RPC method.
pub const SKILLS_GET_METHOD: &str = "skills/get";

/// The optional `resources/directory/read` method, gated behind the
/// extension's `directoryRead` setting. BitRouter declares `directoryRead:
/// false` everywhere and does not implement it; the name is here because the
/// gateway's relay allowlist has to spell it.
pub const RESOURCES_DIRECTORY_READ_METHOD: &str = "resources/directory/read";

/// One file of a skill, paired with the digest of its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillResource {
    /// Resource URI of the file.
    pub uri: String,
    /// SHA-256 of the file's raw bytes, as `sha256:{64 lowercase hex}`.
    pub digest: String,
}

/// A skill entry — identical in shape and meaning in `skills/list` and
/// `skills/get`, per SEP-2640 ("The `skill` object is a skill entry, identical
/// in shape and meaning to an entry of `skills/list`").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// Resource URI of the skill's `SKILL.md`. The final path segment equals
    /// `frontmatter.name`.
    pub uri: String,
    /// The skill's `SKILL.md` YAML frontmatter rendered **verbatim** as JSON —
    /// every field the author wrote, not a curated subset.
    ///
    /// Deliberately an untyped map rather than a struct with `name` /
    /// `description` fields: the SEP requires that fields added by future
    /// revisions of the Agent Skills specification "pass through unchanged",
    /// which a typed struct would silently drop.
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    /// Complete enumeration of the skill's files with their digests.
    ///
    /// `None` — the key omitted entirely — is meaningful and distinct from
    /// `Some(vec![])`: the SEP permits omission *only* for dynamically
    /// generated skills whose content cannot be pre-digested, and such a skill
    /// "offers no content integrity and cannot be content-bound".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Vec<SkillResource>>,
    /// Any *top-level* entry field this crate does not model, preserved
    /// verbatim.
    ///
    /// The same reasoning as `frontmatter` applied one level up: SEP-2640 is a
    /// draft over a spec that evolves separately, so an entry may legitimately
    /// carry fields we have never heard of. Without this catchall, routing an
    /// entry through this type would silently drop them — which for a gateway
    /// means quietly degrading what an upstream published.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The `skills/list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSkillsResult {
    /// The skills this server serves. MAY be empty; per the SEP a host "MUST
    /// NOT treat an empty or partial listing as proof that a server has no
    /// skills".
    pub skills: Vec<SkillEntry>,
}

/// The `skills/get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSkillParams {
    /// URI of a skill's `SKILL.md`.
    pub uri: String,
}

/// The `skills/get` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSkillResult {
    /// The requested entry.
    pub skill: SkillEntry,
}

/// Prefix an upstream skill URI with `label`, moving it into the gateway's
/// namespace. Returns `None` for any URI that is not `skill://`-schemed —
/// those are not aggregated (see the module docs).
pub fn namespace_uri(label: &str, uri: &str) -> Option<String> {
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
    let path = uri
        .strip_prefix(SKILL_SCHEME)?
        .strip_prefix(label)?
        .strip_prefix('/')?;
    Some(format!("{SKILL_SCHEME}{path}"))
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
    let uri = object.get("uri").and_then(|v| v.as_str())?;
    let namespaced = namespace_uri(label, uri)?;
    object.insert("uri".to_string(), namespaced.into());

    // `resources` is the integrity commitment a host's approval binds to, so
    // every entry in it has to move with the skill.
    if let Some(resources) = object.get_mut("resources").and_then(|v| v.as_array_mut()) {
        for resource in resources.iter_mut() {
            let Some(resource) = resource.as_object_mut() else {
                continue;
            };
            let Some(uri) = resource.get("uri").and_then(|v| v.as_str()) else {
                continue;
            };
            // A resource URI outside the skill's own scheme is malformed per
            // the SEP ("Each `uri` MUST be the skill's `SKILL.md` or a file
            // within the skill's directory"); leave it alone rather than
            // inventing a rewrite, and let host-side verification reject it.
            if let Some(namespaced) = namespace_uri(label, uri) {
                resource.insert("uri".to_string(), namespaced.into());
            }
        }
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
            resources: None,
            extra: serde_json::Map::new(),
        }
    }

    /// The SEP's own `skills/list` example must round-trip byte-identically.
    #[test]
    fn sep_example_round_trips() {
        let wire = serde_json::json!({
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
                            "digest": "sha256:a1b2c3d4"
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
                            "digest": "sha256:b2c3d4e5"
                        }
                    ]
                }
            ]
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
            "resources": [{"uri": "skill://x/SKILL.md", "digest": "sha256:aa"}],
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

    /// `None` and `Some(vec![])` are different states and must stay so: the
    /// first says "dynamically generated, no integrity available", the second
    /// would claim a skill has no files at all.
    #[test]
    fn absent_resources_differs_from_empty_resources() {
        let absent = entry("skill://x/SKILL.md", "x");
        let empty = SkillEntry {
            resources: Some(vec![]),
            ..absent.clone()
        };

        let absent_wire = serde_json::to_value(&absent).expect("ser");
        let empty_wire = serde_json::to_value(&empty).expect("ser");
        assert!(
            absent_wire.get("resources").is_none(),
            "omitted, not null: {absent_wire}"
        );
        assert_eq!(empty_wire["resources"], serde_json::json!([]));

        let absent_back: SkillEntry = serde_json::from_value(absent_wire).expect("de");
        let empty_back: SkillEntry = serde_json::from_value(empty_wire).expect("de");
        assert_eq!(absent_back.resources, None);
        assert_eq!(empty_back.resources, Some(vec![]));
    }

    #[test]
    fn get_params_and_result_use_the_sep_field_names() {
        let params: GetSkillParams =
            serde_json::from_value(serde_json::json!({"uri": "skill://pdf/SKILL.md"}))
                .expect("parses");
        assert_eq!(params.uri, "skill://pdf/SKILL.md");

        let result = GetSkillResult {
            skill: entry("skill://pdf/SKILL.md", "pdf"),
        };
        let wire = serde_json::to_value(&result).expect("ser");
        assert_eq!(wire["skill"]["uri"], "skill://pdf/SKILL.md");
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
                {"uri": "skill://refunds/SKILL.md", "digest": "sha256:aa"},
                {"uri": "skill://refunds/examples/email.md", "digest": "sha256:bb"}
            ]
        });
        let out = namespace_entry("acme", &entry).expect("aggregatable");

        assert_eq!(out["uri"], "skill://acme/refunds/SKILL.md");
        assert_eq!(out["resources"][0]["uri"], "skill://acme/refunds/SKILL.md");
        assert_eq!(
            out["resources"][1]["uri"],
            "skill://acme/refunds/examples/email.md"
        );
        // Digests are over content bytes, so rewriting a URI must not disturb
        // them — a changed digest would revoke a host's content-bound approval.
        assert_eq!(out["resources"][0]["digest"], "sha256:aa");
        assert_eq!(out["resources"][1]["digest"], "sha256:bb");
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
    fn entry_without_resources_stays_without_them() {
        // A dynamically generated skill omits `resources`; rewriting must not
        // invent the key, because its absence is meaningful.
        let entry = serde_json::json!({
            "uri": "skill://generated/SKILL.md",
            "frontmatter": {"name": "generated", "description": "d"}
        });
        let out = namespace_entry("srv", &entry).expect("aggregatable");
        assert_eq!(out["uri"], "skill://srv/generated/SKILL.md");
        assert!(out.get("resources").is_none(), "not invented: {out}");
    }
}
