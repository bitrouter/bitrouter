//! Claude Code's `marketplace.json` wire types, shared by the server-side
//! registry and the client installer, plus helpers to fetch and search a
//! registry.
//!
//! The shape mirrors Claude Code's native marketplace manifest:
//! `{ name, owner, plugins: [ { name, source: <union>, description, version,
//! author, keywords, category, tags } ] }`. The per-entry `source` is the
//! structured [`crate::source::Source`] union (`source: "github" | "url" |
//! "git-subdir"`), so a bitrouter registry hub can be added natively in Claude
//! Code AND consumed by this CLI.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Marketplace owner / maintainer (Claude Code `owner`, reused for per-entry
/// `author`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// Display name.
    pub name: String,
    /// Optional contact email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// A single plugin entry in a Claude Code marketplace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    /// Canonical skill name / slug.
    pub name: String,
    /// Structured source (`github` / `url` / `git-subdir`).
    pub source: crate::source::Source,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional version label (informational; resolution uses `source`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Optional maintainer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Owner>,
    /// Discovery keywords.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Optional category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Free-form tags used for `find` filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A Claude Code marketplace manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marketplace {
    /// Marketplace name / slug.
    pub name: String,
    /// Marketplace owner.
    pub owner: Owner,
    /// Available plugins / skills.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<MarketplaceEntry>,
}

impl Marketplace {
    /// Find an entry by exact name.
    pub fn find(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.plugins.iter().find(|e| e.name == name)
    }

    /// Entries whose name, description, keywords, or tags contain `query`
    /// (case-insensitive).
    pub fn search(&self, query: &str) -> Vec<&MarketplaceEntry> {
        let q = query.to_lowercase();
        self.plugins
            .iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q)
                    || e.description
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&q)
                    || e.keywords.iter().any(|k| k.to_lowercase().contains(&q))
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }
}

/// Fetch a namespace's marketplace from a bitrouter registry base URL.
///
/// e.g. base `https://api.bitrouter.ai`, namespace `abc` →
/// `https://api.bitrouter.ai/v1/namespaces/abc/skills/hub`.
pub async fn fetch_marketplace(
    client: &reqwest::Client,
    registry_base: &str,
    namespace_id: &str,
) -> Result<Marketplace> {
    let base = registry_base.trim_end_matches('/');
    let url = format!("{base}/v1/namespaces/{namespace_id}/skills/hub");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Http(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Http(format!("GET {url}: status {}", resp.status())));
    }
    resp.json::<Marketplace>()
        .await
        .map_err(|e| Error::Http(format!("decoding marketplace from {url}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Marketplace {
        serde_json::from_str(
            r#"{
                "name": "acme-skills",
                "owner": {"name": "Acme", "email": "skills@acme.test"},
                "plugins": [
                    {
                        "name": "alpha",
                        "description": "First skill",
                        "source": {"source": "github", "repo": "o/alpha"},
                        "tags": ["cli", "build"]
                    },
                    {
                        "name": "beta",
                        "description": "Second skill",
                        "source": {"source": "github", "repo": "o/beta"},
                        "version": "2.0.0",
                        "author": {"name": "me"}
                    }
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn deserializes_entries() {
        let m = sample();
        assert_eq!(m.name, "acme-skills");
        assert_eq!(m.owner.email.as_deref(), Some("skills@acme.test"));
        assert_eq!(m.plugins.len(), 2);
        assert_eq!(m.plugins[1].version.as_deref(), Some("2.0.0"));
        assert_eq!(
            m.plugins[1].author.as_ref().map(|a| a.name.as_str()),
            Some("me")
        );
        assert!(m.plugins[1].tags.is_empty());
        assert_eq!(
            m.plugins[0].source,
            crate::source::Source::Github {
                repo: "o/alpha".into(),
                r#ref: None,
                sha: None,
            }
        );
    }

    #[test]
    fn find_by_exact_name() {
        let m = sample();
        assert_eq!(
            m.find("beta").map(|e| &e.source),
            Some(&crate::source::Source::Github {
                repo: "o/beta".into(),
                r#ref: None,
                sha: None,
            })
        );
        assert!(m.find("missing").is_none());
    }

    #[test]
    fn search_matches_name_description_and_tags() {
        let m = sample();
        assert_eq!(m.search("first").len(), 1);
        assert_eq!(m.search("build")[0].name, "alpha");
        assert_eq!(m.search("skill").len(), 2);
        assert!(m.search("nope").is_empty());
    }

    #[test]
    fn round_trips_through_json() {
        let m = sample();
        let json = serde_json::to_string(&m).unwrap();
        let back: Marketplace = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn minimal_marketplace_deserializes() {
        let m: Marketplace =
            serde_json::from_str(r#"{"name": "empty", "owner": {"name": "o"}}"#).unwrap();
        assert!(m.plugins.is_empty());
        assert!(m.owner.email.is_none());
    }
}
