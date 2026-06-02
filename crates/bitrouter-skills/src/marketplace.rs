//! The `marketplace.json` wire types shared by the server-side registry and the
//! client installer, plus helpers to fetch and search a registry.
//!
//! This is BitRouter's own registry shape (a flat skill list whose `source`
//! field is any string accepted by [`crate::source::parse_source`]). The
//! Claude Code plugin manifest is a distinct, server-only serialization built
//! on top of these rows.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Public hub endpoint served by a bitrouter registry, relative to its base
/// URL.
const HUB_PATH: &str = "/v1/skills/hub";

/// A single entry in a registry marketplace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    /// Canonical skill name / slug.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Source string, parseable by [`crate::source::parse_source`].
    pub source: String,
    /// Optional version label (informational; resolution uses `source`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Optional maintainer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Free-form tags used for `find` filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// The body of a registry hub / marketplace response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marketplace {
    /// Available skills.
    #[serde(default)]
    pub skills: Vec<MarketplaceEntry>,
}

impl Marketplace {
    /// Find an entry by exact name.
    pub fn find(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.skills.iter().find(|e| e.name == name)
    }

    /// Entries whose name, description, or tags contain `query`
    /// (case-insensitive).
    pub fn search(&self, query: &str) -> Vec<&MarketplaceEntry> {
        let q = query.to_lowercase();
        self.skills
            .iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q)
                    || e.description.to_lowercase().contains(&q)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }
}

/// Fetch the marketplace from a bitrouter registry base URL (e.g.
/// `https://api.bitrouter.ai`).
pub async fn fetch_marketplace(
    client: &reqwest::Client,
    registry_base: &str,
) -> Result<Marketplace> {
    let base = registry_base.trim_end_matches('/');
    let url = format!("{base}{HUB_PATH}");
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
                "skills": [
                    {"name": "alpha", "description": "First skill", "source": "o/alpha", "tags": ["cli", "build"]},
                    {"name": "beta", "description": "Second skill", "source": "o/beta", "version": "2.0.0", "author": "me"}
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn deserializes_entries() {
        let m = sample();
        assert_eq!(m.skills.len(), 2);
        assert_eq!(m.skills[1].version.as_deref(), Some("2.0.0"));
        assert_eq!(m.skills[1].author.as_deref(), Some("me"));
        assert!(m.skills[1].tags.is_empty());
    }

    #[test]
    fn find_by_exact_name() {
        let m = sample();
        assert_eq!(m.find("beta").map(|e| e.source.as_str()), Some("o/beta"));
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
    fn empty_marketplace_default() {
        let m: Marketplace = serde_json::from_str("{}").unwrap();
        assert!(m.skills.is_empty());
    }
}
