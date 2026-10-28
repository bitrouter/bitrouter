//! Agent Skills over MCP wire values (SEP-2640).
//! <https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640>

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

/// The literal value `resources` takes when a skill's content is generated
/// dynamically and therefore cannot be pre-digested.
pub const DYNAMIC_RESOURCES: &str = "dynamic";

/// SEP-2640's per-skill cap on `resources` entries, `SKILL.md` included.
///
/// "Servers SHOULD NOT serve a skill that exceeds either limit; a skill that
/// does is not guaranteed to be loadable by any conforming host."
pub const MAX_SKILL_RESOURCES: usize = 512;

/// SEP-2640's per-skill cap on the sum of `resources[].size`: 16 MiB.
pub const MAX_SKILL_TOTAL_BYTES: u64 = 16_777_216;

/// The only result discriminator valid for the synchronous Skills methods.
///
/// MCP `2026-07-28` permits other result kinds for methods that support MRTR
/// or Tasks. The Skills extension explicitly requires `"complete"` for both
/// `skills/list` and `skills/get`, so representing any other value here would
/// let an origin server construct an invalid result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillsResultType {
    /// The operation completed synchronously.
    Complete,
}

/// Cache visibility for a Skills result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillsCacheScope {
    /// The result is identical for every caller of this server instance.
    Public,
    /// The result may only be reused for the caller that requested it.
    Private,
}

/// One file of a skill, paired with the digest **and byte length** of its
/// bytes.
///
/// `size` is not decoration: the accepted SEP makes it REQUIRED on every entry
/// so a host can budget a skill from the listing alone — enforce
/// [`MAX_SKILL_RESOURCES`] and [`MAX_SKILL_TOTAL_BYTES`] before fetching a
/// single file — and so that "a read whose byte length differs from the entry's
/// `size` is a verification failure equivalent to a digest mismatch, whether or
/// not the host goes on to compute the digest".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillResource {
    /// Resource URI of the file.
    pub uri: String,
    /// SHA-256 of the file's raw bytes, as `sha256:{64 lowercase hex}`.
    pub digest: String,
    /// Length in bytes of the file's raw content — the same bytes `digest`
    /// covers.
    pub size: u64,
}

/// The two forms `skills[].resources` may take.
///
/// The accepted SEP makes the field REQUIRED and admits exactly two values: the
/// complete array, or the literal string `"dynamic"`. "An entry with no
/// `resources` at all, or with any value other than an array or `"dynamic"`, is
/// invalid, and hosts MUST NOT load it" — so this type has no third state, and
/// deserializing anything else is an error rather than a silently tolerated
/// entry a conforming host would refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillResources {
    /// The complete manifest: every file of the skill, each exactly once,
    /// including an entry matching the skill's own `uri`.
    Enumerated(Vec<SkillResource>),
    /// Content generated on demand, so no stable digests can be published. Such
    /// a skill "offers no content integrity and cannot be content-bound".
    Dynamic,
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
    /// Complete enumeration of the skill's files with their digests and sizes,
    /// or [`SkillResources::Dynamic`].
    ///
    /// **Required.** An earlier draft let the key be omitted to mean "generated
    /// dynamically"; the accepted SEP replaced that with an explicit `"dynamic"`
    /// marker precisely "so that a host can tell a deliberately unverifiable
    /// skill from a malformed entry", and made an entry carrying neither form
    /// invalid.
    pub resources: SkillResources,
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
#[serde(rename_all = "camelCase")]
pub struct ListSkillsResult {
    /// Skills list operations always complete synchronously.
    pub result_type: SkillsResultType,
    /// The skills this server serves. MAY be empty; per the SEP a host "MUST
    /// NOT treat an empty or partial listing as proof that a server has no
    /// skills".
    pub skills: Vec<SkillEntry>,
    /// Freshness hint for the listing, in milliseconds.
    pub ttl_ms: u64,
    /// Whether the listing may be reused across callers.
    pub cache_scope: SkillsCacheScope,
}

/// The `skills/get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSkillParams {
    /// URI of a skill's `SKILL.md`.
    pub uri: String,
}

/// The `skills/get` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetSkillResult {
    /// Skill lookups always complete synchronously.
    pub result_type: SkillsResultType,
    /// The requested entry.
    pub skill: SkillEntry,
    /// Freshness hint for the entry, in milliseconds.
    pub ttl_ms: u64,
    /// Whether the entry may be reused across callers.
    pub cache_scope: SkillsCacheScope,
}

impl SkillResources {
    /// The enumerated files, or `None` for a dynamic skill.
    pub fn entries(&self) -> Option<&[SkillResource]> {
        match self {
            Self::Enumerated(entries) => Some(entries),
            Self::Dynamic => None,
        }
    }
}

impl Serialize for SkillResources {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Enumerated(entries) => entries.serialize(serializer),
            Self::Dynamic => serializer.serialize_str(DYNAMIC_RESOURCES),
        }
    }
}

impl<'de> Deserialize<'de> for SkillResources {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Enumerated(Vec<SkillResource>),
            Marker(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Enumerated(entries) => Ok(Self::Enumerated(entries)),
            Wire::Marker(marker) if marker == DYNAMIC_RESOURCES => Ok(Self::Dynamic),
            Wire::Marker(other) => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&other),
                &"an array of resources or the string \"dynamic\"",
            )),
        }
    }
}
