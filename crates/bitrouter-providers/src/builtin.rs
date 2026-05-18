//! Compile-time registry of built-in provider entries.
//!
//! Each TOML under `providers/*.toml` is pulled in via `include_str!`. The
//! list is hand-maintained (rather than a `build.rs`) so adding a provider
//! is a single visible change in this file plus the TOML.

use std::sync::OnceLock;

use crate::LoadError;
use crate::entry::ProviderEntry;

/// One embedded TOML file: `(filename_stem, contents)`. The filename stem
/// MUST match the `id` field inside the TOML (enforced at load time).
const EMBEDDED: &[(&str, &str)] = &[
    ("openai", include_str!("../providers/openai.toml")),
    ("anthropic", include_str!("../providers/anthropic.toml")),
    ("google", include_str!("../providers/google.toml")),
    ("openrouter", include_str!("../providers/openrouter.toml")),
    (
        "github-copilot",
        include_str!("../providers/github-copilot.toml"),
    ),
];

static REGISTRY: OnceLock<Vec<ProviderEntry>> = OnceLock::new();

/// Parse + return every built-in entry. Panics if a TOML fails to parse,
/// duplicates an id, or its declared id differs from its filename — these are
/// programming errors caught by `cargo test`, never user errors.
pub fn all() -> &'static [ProviderEntry] {
    REGISTRY
        .get_or_init(|| load_embedded().expect("built-in provider registry must parse"))
        .as_slice()
}

/// Look up a built-in entry by `id`. Returns `None` for unknown ids.
pub fn find(id: &str) -> Option<&'static ProviderEntry> {
    all().iter().find(|e| e.id == id)
}

/// Parse the embedded slice. Separated from [`all`] so tests can assert on
/// the `Result` instead of catching panics.
pub fn load_embedded() -> Result<Vec<ProviderEntry>, LoadError> {
    let mut out = Vec::with_capacity(EMBEDDED.len());
    for (stem, body) in EMBEDDED {
        let entry: ProviderEntry = toml::from_str(body).map_err(|source| LoadError::Parse {
            id: (*stem).to_string(),
            source,
        })?;
        if entry.id != *stem {
            return Err(LoadError::IdMismatch {
                declared: entry.id,
                expected: (*stem).to_string(),
            });
        }
        if out.iter().any(|e: &ProviderEntry| e.id == entry.id) {
            return Err(LoadError::DuplicateId { id: entry.id });
        }
        out.push(entry);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_parses_cleanly() {
        let entries = load_embedded().expect("embedded TOML files must parse");
        // Bump this when adding a new provider — keeps the test honest about
        // catalog growth.
        assert_eq!(entries.len(), 5);
    }

    #[test]
    fn looks_up_by_id() {
        assert!(find("openai").is_some());
        assert!(find("anthropic").is_some());
        assert!(find("google").is_some());
        assert!(find("openrouter").is_some());
        assert!(find("github-copilot").is_some());
        assert!(find("definitely-not-a-provider").is_none());
    }

    #[test]
    fn github_copilot_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let copilot = find("github-copilot").unwrap();
        // Claude family → Anthropic Messages.
        assert_eq!(
            copilot.api_protocol.resolve("claude-sonnet-4.6"),
            Some(ApiProtocol::Anthropic)
        );
        // GPT-5-codex → OpenAI Responses (chat-completions returns 404 in
        // Copilot for these models).
        assert_eq!(
            copilot.api_protocol.resolve("gpt-5.3-codex"),
            Some(ApiProtocol::Responses)
        );
        // Default → OpenAI Chat Completions.
        assert_eq!(
            copilot.api_protocol.resolve("gpt-4o"),
            Some(ApiProtocol::Openai)
        );
        assert_eq!(
            copilot.api_protocol.resolve("gemini-2.5-pro"),
            Some(ApiProtocol::Openai)
        );
    }

    #[test]
    fn every_entry_has_a_doc_url() {
        for entry in all() {
            assert!(
                entry.doc_url.starts_with("https://"),
                "{} missing https doc_url",
                entry.id
            );
        }
    }
}
