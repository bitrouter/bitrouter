//! End-to-end tests for per-agent routing configuration.
//!
//! Validates that every built-in agent definition produces correct routing
//! env vars and config file patches when paired with a RoutingContext that
//! simulates a running BitRouter instance backed by OpenRouter.

use std::collections::HashMap;

use bitrouter_config::agent_routing::{RoutingContext, extract_provider_keys};
use bitrouter_config::config::{AgentConfig, ProviderConfig};
use bitrouter_config::registry::builtin_agent_defs;

// ── Test helpers ───────────────────────────────────────────────────────

/// Build a RoutingContext that simulates BitRouter at 127.0.0.1:8787
/// with a full set of provider API keys (as an OpenRouter BYOK setup would).
fn test_routing_context() -> RoutingContext {
    let mut keys = HashMap::new();
    keys.insert("OPENAI_API_KEY".to_owned(), "sk-test-openai".to_owned());
    keys.insert(
        "ANTHROPIC_API_KEY".to_owned(),
        "sk-test-anthropic".to_owned(),
    );
    keys.insert("GOOGLE_API_KEY".to_owned(), "sk-test-google".to_owned());
    keys.insert(
        "OPENROUTER_API_KEY".to_owned(),
        "sk-test-openrouter".to_owned(),
    );
    RoutingContext::new("127.0.0.1:8787", &keys)
}

/// Convenience: load the built-in agent definition by name.
fn agent(name: &str) -> AgentConfig {
    builtin_agent_defs()
        .remove(name)
        .unwrap_or_else(|| panic!("built-in agent '{name}' not found"))
}

// ── All agents load successfully ───────────────────────────────────────

#[test]
fn all_builtin_agents_parse() {
    let agents = builtin_agent_defs();
    let expected = [
        "claude",
        "codex",
        "cline",
        "copilot",
        "deepagents",
        "gemini",
        "goose",
        "hermes",
        "kilo",
        "openclaw",
        "opencode",
        "openhands",
        "pi",
    ];
    for name in &expected {
        assert!(agents.contains_key(*name), "missing built-in agent: {name}");
    }
    assert_eq!(
        agents.len(),
        expected.len(),
        "unexpected agent count: got {}, expected {}",
        agents.len(),
        expected.len()
    );
}

// ── Provider key extraction ────────────────────────────────────────────

#[test]
fn extract_provider_keys_from_openrouter_config() {
    let mut providers = HashMap::new();
    providers.insert(
        "openrouter".to_owned(),
        ProviderConfig {
            api_key: Some("sk-or-test".to_owned()),
            ..Default::default()
        },
    );
    providers.insert(
        "openai".to_owned(),
        ProviderConfig {
            api_key: Some("sk-openai".to_owned()),
            ..Default::default()
        },
    );
    providers.insert(
        "anthropic".to_owned(),
        ProviderConfig {
            api_key: Some("sk-ant".to_owned()),
            ..Default::default()
        },
    );
    providers.insert(
        "google".to_owned(),
        ProviderConfig {
            api_key: Some("sk-google".to_owned()),
            ..Default::default()
        },
    );

    let keys = extract_provider_keys(&providers);
    assert_eq!(
        keys.get("OPENROUTER_API_KEY").map(String::as_str),
        Some("sk-or-test")
    );
    assert_eq!(
        keys.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-openai")
    );
    assert_eq!(
        keys.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-ant")
    );
    assert_eq!(
        keys.get("GOOGLE_API_KEY").map(String::as_str),
        Some("sk-google")
    );
}

// ── Per-agent env var resolution ──────────────────────────────────────
//
// For each agent with a `routing.env` block, verify that the RoutingContext
// resolves all variables correctly and that empty values are filtered out.

#[test]
fn claude_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("claude");
    let routing = cfg.routing.as_ref().expect("claude should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1"),
        "claude: ANTHROPIC_BASE_URL should point to bitrouter /v1"
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic"),
        "claude: ANTHROPIC_API_KEY should be resolved"
    );
}

#[test]
fn codex_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("codex");
    let routing = cfg.routing.as_ref().expect("codex should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("OPENAI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1"),
        "codex: OPENAI_BASE_URL should point to bitrouter /v1"
    );
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai"),
        "codex: OPENAI_API_KEY should be resolved"
    );
}

#[test]
fn cline_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("cline");
    let routing = cfg.routing.as_ref().expect("cline should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic"),
    );
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai"),
    );
}

#[test]
fn goose_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("goose");
    let routing = cfg.routing.as_ref().expect("goose should have routing");
    let env = ctx.resolve_env(routing);

    // Goose uses OPENAI_HOST / ANTHROPIC_HOST (no /v1 suffix)
    assert_eq!(
        env.get("OPENAI_HOST").map(String::as_str),
        Some("http://127.0.0.1:8787"),
        "goose: OPENAI_HOST should be BITROUTER_URL (no /v1)"
    );
    assert_eq!(
        env.get("ANTHROPIC_HOST").map(String::as_str),
        Some("http://127.0.0.1:8787"),
        "goose: ANTHROPIC_HOST should be BITROUTER_URL (no /v1)"
    );
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn openclaw_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("openclaw");
    let routing = cfg.routing.as_ref().expect("openclaw should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("OPENAI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1")
    );
    assert_eq!(
        env.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1")
    );
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn deepagents_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("deepagents");
    let routing = cfg
        .routing
        .as_ref()
        .expect("deepagents should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("OPENAI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1")
    );
    assert_eq!(
        env.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1")
    );
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn opencode_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("opencode");
    let routing = cfg.routing.as_ref().expect("opencode should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("LOCAL_ENDPOINT").map(String::as_str),
        Some("http://127.0.0.1:8787/v1"),
        "opencode: LOCAL_ENDPOINT should point to bitrouter /v1"
    );
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn gemini_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("gemini");
    let routing = cfg.routing.as_ref().expect("gemini should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("GEMINI_API_KEY").map(String::as_str),
        Some("sk-test-google"),
        "gemini: GEMINI_API_KEY should resolve from GOOGLE_API_KEY"
    );
    // Gemini has no base URL override
    assert!(!env.contains_key("OPENAI_BASE_URL"));
    assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
}

#[test]
fn hermes_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("hermes");
    let routing = cfg.routing.as_ref().expect("hermes should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn openhands_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("openhands");
    let routing = cfg.routing.as_ref().expect("openhands should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn pi_routing_env() {
    let ctx = test_routing_context();
    let cfg = agent("pi");
    let routing = cfg.routing.as_ref().expect("pi should have routing");
    let env = ctx.resolve_env(routing);

    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test-openai")
    );
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-test-anthropic")
    );
}

#[test]
fn copilot_has_no_routing() {
    let cfg = agent("copilot");
    assert!(
        cfg.routing.is_none(),
        "copilot should have no routing (GitHub OAuth only)"
    );
}

// ── Agents without keys still get filtered ─────────────────────────────
//
// When a provider key is NOT in the context, the resolved env var should
// be empty and therefore filtered out (not injected into the subprocess).

#[test]
fn missing_keys_are_filtered() {
    // Context with NO provider keys at all
    let ctx = RoutingContext::new("127.0.0.1:8787", &HashMap::new());
    let cfg = agent("codex");
    let routing = cfg.routing.as_ref().expect("codex should have routing");
    let env = ctx.resolve_env(routing);

    // Base URL still resolves (it uses BITROUTER_URL_V1, always available)
    assert_eq!(
        env.get("OPENAI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:8787/v1")
    );
    // API key should NOT be present (empty value filtered)
    assert!(
        !env.contains_key("OPENAI_API_KEY"),
        "missing key should be filtered out"
    );
}

// ── Config file patching ──────────────────────────────────────────────

#[test]
fn codex_config_file_toml_patch() {
    let ctx = test_routing_context();
    let cfg = agent("codex");
    let routing = cfg.routing.as_ref().expect("codex should have routing");
    assert!(
        !routing.config_files.is_empty(),
        "codex should have config_files"
    );

    let dir = tempfile::tempdir().expect("create tempdir");
    let toml_path = dir.path().join("config.toml");

    // Simulate patching by rewriting path to a temp location
    let patch = &routing.config_files[0];
    assert_eq!(
        patch.format,
        bitrouter_config::config::ConfigFileFormat::Toml
    );

    // Apply patch to temp path
    let results = ctx.apply_config_patches(&[bitrouter_config::config::ConfigFilePatch {
        path: toml_path.to_string_lossy().into_owned(),
        format: patch.format,
        values: patch.values.clone(),
    }]);

    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.is_ok(),
        "TOML patch failed: {:?}",
        results[0].1
    );

    let raw = std::fs::read_to_string(&toml_path).expect("read patched TOML");
    let doc: toml_edit::DocumentMut = raw.parse().expect("parse TOML");
    assert_eq!(
        doc["openai_base_url"].as_str(),
        Some("http://127.0.0.1:8787/v1"),
        "codex TOML: openai_base_url should be resolved"
    );
}

#[test]
fn cline_config_file_json_patch() {
    let ctx = test_routing_context();
    let cfg = agent("cline");
    let routing = cfg.routing.as_ref().expect("cline should have routing");
    assert!(
        !routing.config_files.is_empty(),
        "cline should have config_files"
    );

    let dir = tempfile::tempdir().expect("create tempdir");
    let json_path = dir.path().join("globalState.json");

    let patch = &routing.config_files[0];
    assert_eq!(
        patch.format,
        bitrouter_config::config::ConfigFileFormat::Json
    );

    let results = ctx.apply_config_patches(&[bitrouter_config::config::ConfigFilePatch {
        path: json_path.to_string_lossy().into_owned(),
        format: patch.format,
        values: patch.values.clone(),
    }]);

    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.is_ok(),
        "JSON patch failed: {:?}",
        results[0].1
    );

    let raw = std::fs::read_to_string(&json_path).expect("read patched JSON");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse JSON");
    assert_eq!(
        doc["anthropicBaseUrl"].as_str(),
        Some("http://127.0.0.1:8787/v1"),
        "cline JSON: anthropicBaseUrl should be resolved"
    );
    assert_eq!(
        doc["openAiBaseUrl"].as_str(),
        Some("http://127.0.0.1:8787/v1"),
        "cline JSON: openAiBaseUrl should be resolved"
    );
}

#[test]
fn kilo_config_file_json_patch() {
    let ctx = test_routing_context();
    let cfg = agent("kilo");
    let routing = cfg.routing.as_ref().expect("kilo should have routing");
    assert!(
        !routing.config_files.is_empty(),
        "kilo should have config_files"
    );

    let dir = tempfile::tempdir().expect("create tempdir");
    let json_path = dir.path().join("opencode.json");

    let patch = &routing.config_files[0];
    assert_eq!(
        patch.format,
        bitrouter_config::config::ConfigFileFormat::Json
    );

    let results = ctx.apply_config_patches(&[bitrouter_config::config::ConfigFilePatch {
        path: json_path.to_string_lossy().into_owned(),
        format: patch.format,
        values: patch.values.clone(),
    }]);

    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.is_ok(),
        "JSON patch failed: {:?}",
        results[0].1
    );

    let raw = std::fs::read_to_string(&json_path).expect("read patched JSON");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse JSON");
    assert_eq!(
        doc["provider"]["bitrouter"]["api"].as_str(),
        Some("http://127.0.0.1:8787/v1"),
        "kilo JSON: provider.bitrouter.api should be resolved via dot-notation"
    );
}

// ── Config file patching preserves existing content ────────────────────

#[test]
fn json_patch_preserves_existing_keys() {
    let ctx = test_routing_context();
    let dir = tempfile::tempdir().expect("create tempdir");
    let json_path = dir.path().join("existing.json");

    // Pre-populate with existing content
    std::fs::write(
        &json_path,
        r#"{"existingKey": "keep-me", "anthropicBaseUrl": "old-value"}"#,
    )
    .expect("write existing JSON");

    let cfg = agent("cline");
    let routing = cfg.routing.as_ref().expect("cline has routing");
    let patch = &routing.config_files[0];

    let results = ctx.apply_config_patches(&[bitrouter_config::config::ConfigFilePatch {
        path: json_path.to_string_lossy().into_owned(),
        format: patch.format,
        values: patch.values.clone(),
    }]);
    assert!(results[0].1.is_ok());

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_path).expect("read")).expect("parse");
    assert_eq!(
        doc["existingKey"].as_str(),
        Some("keep-me"),
        "existing key preserved"
    );
    assert_eq!(
        doc["anthropicBaseUrl"].as_str(),
        Some("http://127.0.0.1:8787/v1"),
        "patched key updated"
    );
}

// ── Full agent routing matrix ──────────────────────────────────────────
//
// Comprehensive test: for every agent that has routing, verify that the
// resolved env contains at least one base URL or API key pointing to
// bitrouter. This catches regressions from YAML typos.

#[test]
fn all_routable_agents_produce_valid_env() {
    let ctx = test_routing_context();
    let agents = builtin_agent_defs();

    let routable_agents = [
        "claude",
        "codex",
        "cline",
        "deepagents",
        "gemini",
        "goose",
        "hermes",
        "openclaw",
        "opencode",
        "openhands",
        "pi",
    ];

    for name in &routable_agents {
        let cfg = agents
            .get(*name)
            .unwrap_or_else(|| panic!("agent '{name}' not found"));
        let routing = cfg
            .routing
            .as_ref()
            .unwrap_or_else(|| panic!("agent '{name}' should have routing"));
        let env = ctx.resolve_env(routing);

        assert!(
            !env.is_empty(),
            "agent '{name}': resolved env should not be empty"
        );

        // Every routable agent should inject at least one key that
        // references either the BitRouter URL or a provider API key.
        let has_url = env.values().any(|v| v.contains("127.0.0.1:8787"));
        let has_key = env.values().any(|v| v.starts_with("sk-test-"));
        assert!(
            has_url || has_key,
            "agent '{name}': env should contain bitrouter URL or API key, got: {env:?}"
        );
    }
}

// ── Agents with full routing have base URL ─────────────────────────────

#[test]
fn full_routing_agents_inject_base_url() {
    let ctx = test_routing_context();
    let agents = builtin_agent_defs();

    // Agents that should inject a base URL (not just API keys)
    let full_routing = [
        ("claude", "ANTHROPIC_BASE_URL"),
        ("codex", "OPENAI_BASE_URL"),
        ("deepagents", "OPENAI_BASE_URL"),
        ("goose", "OPENAI_HOST"),
        ("openclaw", "OPENAI_BASE_URL"),
        ("opencode", "LOCAL_ENDPOINT"),
    ];

    for (name, url_var) in &full_routing {
        let cfg = agents
            .get(*name)
            .unwrap_or_else(|| panic!("agent '{name}' not found"));
        let routing = cfg.routing.as_ref().unwrap();
        let env = ctx.resolve_env(routing);

        let url = env
            .get(*url_var)
            .unwrap_or_else(|| panic!("agent '{name}': missing {url_var}"));
        assert!(
            url.contains("127.0.0.1:8787"),
            "agent '{name}': {url_var} should point to bitrouter, got: {url}"
        );
    }
}

// ── Partial routing agents inject keys only ────────────────────────────

#[test]
fn partial_routing_agents_inject_keys_only() {
    let ctx = test_routing_context();
    let agents = builtin_agent_defs();

    // Agents that only inject API keys (no base URL override)
    let key_only = ["gemini", "hermes", "openhands", "pi"];

    for name in &key_only {
        let cfg = agents
            .get(*name)
            .unwrap_or_else(|| panic!("agent '{name}' not found"));
        let routing = cfg.routing.as_ref().unwrap();
        let env = ctx.resolve_env(routing);

        // Should have at least one API key
        assert!(
            env.values().any(|v| v.starts_with("sk-test-")),
            "agent '{name}': should have at least one API key"
        );

        // Should NOT have a base URL pointing to bitrouter
        let has_base_url = env
            .iter()
            .any(|(k, _)| k.contains("BASE_URL") || k.contains("HOST") || k == "LOCAL_ENDPOINT");
        assert!(
            !has_base_url,
            "agent '{name}': should not have a base URL env var, got: {env:?}"
        );
    }
}

// ── Kilo: config-file-only routing ─────────────────────────────────────

#[test]
fn kilo_uses_config_file_not_env() {
    let ctx = test_routing_context();
    let cfg = agent("kilo");
    let routing = cfg.routing.as_ref().expect("kilo should have routing");

    // Kilo has no env vars for routing
    let env = ctx.resolve_env(routing);
    assert!(
        env.is_empty(),
        "kilo: should have no env vars, got: {env:?}"
    );

    // But has config file patches
    assert!(
        !routing.config_files.is_empty(),
        "kilo: should have config_files"
    );
}
