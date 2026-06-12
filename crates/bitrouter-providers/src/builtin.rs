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
///
/// Order matters for user-facing output: callers that iterate this list to
/// render a list of providers (e.g. the zero-config onboarding hint) take
/// the first entry as the recommended default. `bitrouter` is deliberately
/// first — it is the project's official hosted gateway and gives a new
/// user one credential covering every supported model. The id is the
/// short, brand-aligned form so model addressing reads naturally:
/// `bitrouter:gpt-5.5`, `bitrouter:claude-sonnet-4.6`, …
const EMBEDDED: &[(&str, &str)] = &[
    ("bitrouter", include_str!("../providers/bitrouter.toml")),
    ("openai", include_str!("../providers/openai.toml")),
    (
        "openai-codex",
        include_str!("../providers/openai-codex.toml"),
    ),
    ("anthropic", include_str!("../providers/anthropic.toml")),
    ("google", include_str!("../providers/google.toml")),
    ("openrouter", include_str!("../providers/openrouter.toml")),
    (
        "github-copilot",
        include_str!("../providers/github-copilot.toml"),
    ),
    (
        "opencode-zen",
        include_str!("../providers/opencode-zen.toml"),
    ),
    ("opencode-go", include_str!("../providers/opencode-go.toml")),
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
        assert_eq!(entries.len(), 9);
    }

    #[test]
    fn bitrouter_is_first_for_onboarding_priority() {
        let entries = load_embedded().expect("embedded TOML files must parse");
        assert_eq!(
            entries.first().map(|e| e.id.as_str()),
            Some("bitrouter"),
            "`bitrouter` must lead the catalog so the zero-config hint \
             recommends the hosted gateway first"
        );
    }

    #[test]
    fn bitrouter_parses_with_bearer_env_var() {
        let entry = find("bitrouter").expect("`bitrouter` must be in the catalog");
        assert_eq!(entry.api_base, "https://api.bitrouter.ai/v1");
        assert_eq!(entry.auth.env_var(), Some("BITROUTER_API_KEY"));
        use bitrouter_sdk::language_model::types::ApiProtocol;
        assert_eq!(
            entry.api_protocol.resolve("gpt-4o"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn openai_advertises_chat_and_responses() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        // OpenAI serves the same models over both Chat Completions and the
        // Responses API at one base URL. Advertising the ordered set lets
        // protocol-native routing honour an inbound Responses request without
        // per-request config, while Chat Completions stays the preferred head
        // (the default for any other inbound protocol).
        let openai = find("openai").unwrap();
        assert_eq!(
            openai.api_protocol.resolve("gpt-5.5"),
            Some(vec![ApiProtocol::ChatCompletions, ApiProtocol::Responses])
        );
    }

    #[test]
    fn looks_up_by_id() {
        assert!(find("bitrouter").is_some());
        assert!(find("openai").is_some());
        assert!(find("openai-codex").is_some());
        assert!(find("anthropic").is_some());
        assert!(find("google").is_some());
        assert!(find("openrouter").is_some());
        assert!(find("github-copilot").is_some());
        assert!(find("opencode-zen").is_some());
        assert!(find("opencode-go").is_some());
        assert!(find("definitely-not-a-provider").is_none());
    }

    #[test]
    fn opencode_zen_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let zen = find("opencode-zen").unwrap();
        // GPT family → Responses (zen serves them via /responses).
        assert_eq!(
            zen.api_protocol.resolve("opencode/gpt-5.5"),
            Some(vec![ApiProtocol::Responses])
        );
        assert_eq!(
            zen.api_protocol.resolve("opencode/gpt-5.3-codex"),
            Some(vec![ApiProtocol::Responses])
        );
        // Claude family → Messages.
        assert_eq!(
            zen.api_protocol.resolve("opencode/claude-opus-4.7"),
            Some(vec![ApiProtocol::Messages])
        );
        // Gemini family → Google.
        assert_eq!(
            zen.api_protocol.resolve("opencode/gemini-3.1-pro"),
            Some(vec![ApiProtocol::GenerateContent])
        );
        // Everything else (qwen, glm, kimi, minimax, …) → Chat Completions.
        assert_eq!(
            zen.api_protocol.resolve("opencode/qwen3.6-plus"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            zen.api_protocol.resolve("opencode/minimax-m2.7"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn opencode_go_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let go = find("opencode-go").unwrap();
        // MiniMax → Messages (go serves MiniMax via /messages).
        assert_eq!(
            go.api_protocol.resolve("opencode-go/minimax-m2.7"),
            Some(vec![ApiProtocol::Messages])
        );
        // Everyone else (glm, kimi, deepseek, mimo, qwen) → Chat Completions.
        assert_eq!(
            go.api_protocol.resolve("opencode-go/glm-5.1"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            go.api_protocol.resolve("opencode-go/kimi-k2.6"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            go.api_protocol.resolve("opencode-go/deepseek-v4-pro"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn opencode_zen_and_go_share_one_env_var() {
        // The user opens *one* opencode.ai account; both gateway tiers
        // authenticate with the same `OPENCODE_ZEN_API_KEY`, so a
        // subscriber to Go gets Zen pay-as-you-go billing fall-through
        // (and vice versa) without juggling two creds.
        assert_eq!(
            find("opencode-zen").unwrap().auth.env_var(),
            Some("OPENCODE_ZEN_API_KEY")
        );
        assert_eq!(
            find("opencode-go").unwrap().auth.env_var(),
            Some("OPENCODE_ZEN_API_KEY")
        );
    }

    #[test]
    fn github_copilot_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let copilot = find("github-copilot").unwrap();
        // Claude family → Messages.
        assert_eq!(
            copilot.api_protocol.resolve("claude-sonnet-4.6"),
            Some(vec![ApiProtocol::Messages])
        );
        // GPT-5-codex → Responses (chat-completions returns 404 in
        // Copilot for these models).
        assert_eq!(
            copilot.api_protocol.resolve("gpt-5.3-codex"),
            Some(vec![ApiProtocol::Responses])
        );
        // Default → Chat Completions.
        assert_eq!(
            copilot.api_protocol.resolve("gpt-4o"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            copilot.api_protocol.resolve("gemini-2.5-pro"),
            Some(vec![ApiProtocol::ChatCompletions])
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
