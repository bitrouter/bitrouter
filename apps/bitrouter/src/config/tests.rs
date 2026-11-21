//! Config parsing + `${VAR}` substitution tests.

use super::*;
use bitrouter_ai::routing::SortOrder;

#[test]
fn decision_model_config_validates_bounds_without_loading_credentials()
-> crate::gateway::error::Result<()> {
    let config = parse_with(
        "inherit_defaults: false\ndecision_model:\n  model: fixture-decision\n",
        |_| None,
    )?;
    let decision = config
        .decision_model
        .ok_or_else(|| BitrouterError::internal("missing decision config"))?;
    assert_eq!(decision.api_key_env, "TYPESAFE_API_KEY");
    assert_eq!(decision.base_url, "https://api.typesafe.ai");
    for settings in [
        "model: ''",
        "model: fixture\n  timeout_ms: 0",
        "model: fixture\n  max_response_bytes: 0",
        "model: fixture\n  base_url: file:///tmp/model",
        "model: fixture\n  policy: {confidence_threshold: 0.1}",
        "model: fixture\n  policy: {max_request_bytes: 0}",
        "model: fixture\n  pricing: {input_usd_per_million: -1, output_usd_per_million: 0}",
        "model: fixture\n  credential: do-not-inline-secrets",
    ] {
        let yaml = format!("inherit_defaults: false\ndecision_model:\n  {settings}\n");
        assert!(
            parse_with(&yaml, |_| None).is_err(),
            "accepted invalid decision settings"
        );
    }
    Ok(())
}
use bitrouter_ai::types::{ApiProtocol, ReasoningEffort};

#[test]
fn request_checker_config_applies_binding_defaults() -> crate::gateway::error::Result<()> {
    let config = parse_with(
        r#"
inherit_defaults: false
checkers:
  safety:
    native:
      revision: rules-v1
routers:
  guarded:
    selection:
      kind: model
      model: vendor:model
    checks:
      request:
        - checker: safety
"#,
        |_| None,
    )?;

    let binding = config.routers["guarded"]
        .checks
        .request
        .first()
        .ok_or_else(|| BitrouterError::internal("checker binding was not parsed"))?;
    assert_eq!(binding.timeout_ms, router::DEFAULT_CHECKER_TIMEOUT_MS);
    assert_eq!(
        binding.max_input_bytes,
        router::DEFAULT_CHECKER_MAX_INPUT_BYTES
    );
    let identity = config
        .resolve_router("bitrouter/guarded")?
        .router
        .ok_or_else(|| BitrouterError::internal("router identity was not resolved"))?;
    assert!(identity.binding_digest.starts_with("router-v2:sha256:"));
    assert!(!identity.binding_digest.contains("rules-v1"));
    Ok(())
}

#[test]
fn request_checker_config_rejects_invalid_static_bindings() -> crate::gateway::error::Result<()> {
    let invalid = [
        ("native: {revision: ''}", "native revision"),
        ("native: {revision: 'has spaces'}", "native revision"),
        (
            "endpoint: https://checker.example/check\n    contract_version: 1",
            "unknown field",
        ),
        (
            "native: {revision: v1}\n    endpoint: null",
            "unknown field",
        ),
        ("credential_env: TOKEN", "unknown field"),
        ("contract_version: 1", "unknown field"),
        ("native: {revision: v1}\n    typo: true", "unknown field"),
    ];
    for (checker_fields, expected) in invalid {
        let yaml = format!("inherit_defaults: false\ncheckers:\n  safety:\n    {checker_fields}\n");
        let error = parse_with(&yaml, |_| None)
            .err()
            .ok_or_else(|| BitrouterError::internal("invalid checker config was accepted"))?;
        assert!(error.to_string().contains(expected), "got: {error}");
    }
    Ok(())
}

#[test]
fn request_checker_config_rejects_unknown_refs_and_invalid_limits()
-> crate::gateway::error::Result<()> {
    let invalid = [
        ("checker: missing", "unknown checker"),
        ("checker: safety\n          timeout_ms: 0", "timeout_ms"),
        (
            "checker: safety\n          max_input_bytes: 4194305",
            "max_input_bytes",
        ),
    ];
    for (binding, expected) in invalid {
        let yaml = format!(
            r#"
inherit_defaults: false
checkers:
  safety:
    native:
      revision: rules-v1
routers:
  guarded:
    selection:
      kind: model
      model: vendor:model
    checks:
      request:
        - {binding}
"#
        );
        let error = parse_with(&yaml, |_| None)
            .err()
            .ok_or_else(|| BitrouterError::internal("invalid checker binding was accepted"))?;
        assert!(error.to_string().contains(expected), "got: {error}");
    }
    Ok(())
}

#[test]
fn request_checker_revision_changes_router_binding_digest() -> crate::gateway::error::Result<()> {
    fn digest(revision: &str) -> crate::gateway::error::Result<String> {
        let yaml = format!(
            r#"
inherit_defaults: false
checkers:
  safety:
    native:
      revision: {revision}
routers:
  guarded:
    selection:
      kind: model
      model: vendor:model
    checks:
      request:
        - checker: safety
"#
        );
        parse_with(&yaml, |_| None)?
            .resolve_router("bitrouter/guarded")?
            .router
            .map(|identity| identity.binding_digest)
            .ok_or_else(|| BitrouterError::internal("router identity was not resolved"))
    }

    assert_ne!(digest("rules-v1")?, digest("rules-v2")?);
    Ok(())
}

#[test]
fn provider_headers_accept_static_and_passthrough_forms() -> crate::gateway::error::Result<()> {
    let config = parse_with(
        r#"
inherit_defaults: false
providers:
  opencode-go:
    api_base: https://opencode.ai/zen/go/v1
    api_key: test
    headers:
      x-opencode-session:
        default: claude-code
        passthrough: true
      user-agent: my-agent/1.0
      x-rejected:
        passthrough: false
"#,
        |_| None,
    )?;

    let chain = routing_table::resolve_route_chain(
        &config,
        "opencode-go:test-model",
        &crate::gateway::pipeline::routing::RoutingPrefs::default(),
    )?;
    let target = chain
        .first()
        .ok_or_else(|| BitrouterError::internal("provider route was empty"))?;
    let session = target
        .headers
        .iter()
        .find(|rule| rule.name() == "x-opencode-session")
        .ok_or_else(|| BitrouterError::internal("session header rule was not routed"))?;
    assert!(session.passthrough());
    assert_eq!(
        session.default().and_then(|value| value.to_str().ok()),
        Some("claude-code")
    );
    let user_agent = target
        .headers
        .iter()
        .find(|rule| rule.name() == "user-agent")
        .ok_or_else(|| BitrouterError::internal("user-agent rule was not routed"))?;
    assert!(!user_agent.passthrough());
    assert_eq!(
        user_agent.default().and_then(|value| value.to_str().ok()),
        Some("my-agent/1.0")
    );
    let rejected = target
        .headers
        .iter()
        .find(|rule| rule.name() == "x-rejected")
        .ok_or_else(|| BitrouterError::internal("reject rule was not routed"))?;
    assert!(!rejected.passthrough());
    assert!(rejected.default().is_none());
    Ok(())
}

#[test]
fn provider_headers_are_inherited_when_child_declares_none() -> crate::gateway::error::Result<()> {
    let config = parse_with(
        r#"
inherit_defaults: false
providers:
  parent:
    api_base: https://parent.example/v1
    headers:
      x-provider-version: v1
  child:
    derives: parent
    api_base: https://child.example/v1
"#,
        |_| None,
    )?;
    assert_eq!(
        config.providers["child"].headers,
        config.providers["parent"].headers
    );
    Ok(())
}

#[test]
fn provider_headers_reject_invalid_reserved_and_duplicate_names() {
    for (headers, expected) in [
        ("'bad header': value", "invalid provider header name"),
        (
            "x-test: \"line\\nbreak\"",
            "invalid value for provider header 'x-test'",
        ),
        (
            "authorization: value",
            "provider header 'authorization' is reserved",
        ),
        (
            "x-bitrouter-adapter-attempt: value",
            "provider header 'x-bitrouter-adapter-attempt' is reserved",
        ),
        (
            "x-bitrouter-adapter-outcome: value",
            "provider header 'x-bitrouter-adapter-outcome' is reserved",
        ),
        (
            "User-Agent: first\n      user-agent: second",
            "duplicate provider header 'user-agent'",
        ),
        (
            "x-test:\n        default: value\n        typo: true",
            "did not match any variant",
        ),
    ] {
        let yaml = format!(
            "inherit_defaults: false\nproviders:\n  test:\n    api_base: https://api.example/v1\n    headers:\n      {headers}\n"
        );
        let error = parse_with(&yaml, |_| None)
            .err()
            .unwrap_or_else(|| BitrouterError::internal("invalid provider header was accepted"));
        assert!(error.to_string().contains(expected), "got: {error}");
    }
}

#[test]
fn named_policy_accepts_explicit_model_and_effort() -> anyhow::Result<()> {
    let config = serde_saphyr::from_str::<bitrouter_ai::policy::Policy>(
        r#"
  tiers:
    strong:
      model: openai-codex:gpt-5.6-sol
      effort: high
    economy: { model: openai-codex:gpt-5.6-sol }
  default_tier: strong
"#,
    )?;

    let strong = config
        .tiers
        .get("strong")
        .ok_or_else(|| BitrouterError::internal("strong tier was not parsed"))?;
    assert_eq!(strong.model(), "openai-codex:gpt-5.6-sol");
    assert_eq!(strong.effort(), Some(ReasoningEffort::High));

    let economy = config
        .tiers
        .get("economy")
        .ok_or_else(|| BitrouterError::internal("economy tier was not parsed"))?;
    assert_eq!(economy.model(), "openai-codex:gpt-5.6-sol");
    assert_eq!(economy.effort(), None);
    Ok(())
}

#[test]
fn policy_table_rejects_unknown_compound_target_fields() -> anyhow::Result<()> {
    let error = serde_saphyr::from_str::<bitrouter_ai::policy::Policy>(
        r#"
  tiers:
    strong:
      model: openai-codex:gpt-5.6-sol
      effort: high
      typo: ignored-would-be-unsafe
  default_tier: strong
"#,
    )
    .err()
    .ok_or_else(|| BitrouterError::internal("unknown target field was accepted"))?;
    assert!(error.to_string().contains("unknown field"));
    Ok(())
}

#[test]
fn provider_model_declares_an_exact_reasoning_effort_subset() -> crate::gateway::error::Result<()> {
    let config = parse_with(
        r#"
inherit_defaults: false
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: test
    models:
      - id: openai/gpt-test
        capabilities: [reasoning]
        reasoning_effort:
          levels: [low, medium, high]
          default: high
"#,
        |_| None,
    )?;
    let model = config.providers["openai"]
        .models
        .first()
        .ok_or_else(|| BitrouterError::internal("parsed provider has no model"))?;
    let effort = model
        .reasoning_effort
        .as_ref()
        .ok_or_else(|| BitrouterError::internal("model lost reasoning_effort"))?;
    assert_eq!(
        effort.levels,
        vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ]
    );
    assert_eq!(effort.default, Some(ReasoningEffort::High));
    Ok(())
}

#[test]
fn provider_model_rejects_a_reasoning_effort_default_outside_its_levels()
-> crate::gateway::error::Result<()> {
    let error = parse_with(
        r#"
inherit_defaults: false
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: test
    models:
      - id: openai/gpt-test
        capabilities: [reasoning]
        reasoning_effort:
          levels: [low, medium]
          default: high
"#,
        |_| None,
    )
    .err()
    .ok_or_else(|| BitrouterError::internal("invalid model effort config was accepted"))?;
    assert!(
        error
            .to_string()
            .contains("reasoning_effort.default 'high' must be listed in levels"),
        "got: {error}"
    );
    Ok(())
}

#[test]
fn provider_model_rejects_malformed_reasoning_effort_capabilities()
-> crate::gateway::error::Result<()> {
    for (model_yaml, expected) in [
        (
            "capabilities: [reasoning]\n        reasoning_effort:\n          levels: []",
            "reasoning_effort.levels must not be empty",
        ),
        (
            "capabilities: [reasoning]\n        reasoning_effort:\n          levels: [low, low]",
            "reasoning_effort.levels contains duplicate 'low'",
        ),
        (
            "reasoning_effort:\n          levels: [low, high]",
            "reasoning_effort requires the reasoning capability",
        ),
    ] {
        let yaml = format!(
            "inherit_defaults: false\nproviders:\n  test:\n    api_base: https://api.example.com/v1\n    api_key: test\n    models:\n      - id: acme/model\n        {model_yaml}\n"
        );
        let error = parse_with(&yaml, |_| None)
            .err()
            .ok_or_else(|| BitrouterError::internal("invalid effort capability was accepted"))?;
        assert!(error.to_string().contains(expected), "got: {error}");
    }
    Ok(())
}

#[test]
fn defaults_are_sane() {
    let cfg = Config::default();
    assert_eq!(cfg.server.listen, "0.0.0.0:4356");
    assert!(!cfg.control.enabled);
    assert_eq!(cfg.control.listen, "127.0.0.1:4358");
    assert!(
        !cfg.server.skip_auth,
        "skip_auth code default must be false"
    );
    assert!(cfg.inherit_defaults);
    assert!(!cfg.trajectory.enabled);
    assert_eq!(cfg.trajectory.retention_days, 30);
    assert_eq!(cfg.trajectory.outbox_batch_size, 100);
    assert_eq!(cfg.continuation.retention_days, 30);
    assert_eq!(cfg.continuation.prune_batch_size, 1_000);
}

#[test]
fn remote_control_is_explicitly_opt_in_and_keeps_loopback_default()
-> crate::gateway::error::Result<()> {
    let cfg = parse_with(
        r#"
control:
  enabled: true
"#,
        |_| None,
    )?;
    assert!(cfg.control.enabled);
    assert_eq!(cfg.control.listen, "127.0.0.1:4358");
    Ok(())
}

#[test]
fn continuation_config_is_always_active_round_trippable_and_bounded() {
    let config = parse_with(
        "continuation:\n  retention_days: 45\n  prune_batch_size: 250\n",
        |_| None,
    )
    .unwrap();
    assert_eq!(config.continuation.retention_days, 45);
    assert_eq!(config.continuation.prune_batch_size, 250);

    let encoded = serde_saphyr::to_string(&config.continuation).unwrap();
    let decoded: ContinuationConfig = serde_saphyr::from_str(&encoded).unwrap();
    assert_eq!(decoded, config.continuation);

    let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
    let continuation = &schema["properties"]["continuation"];
    assert!(!continuation.is_null());
    let rendered = serde_json::to_string(continuation).unwrap();
    assert!(rendered.contains("retention_days"));
    assert!(rendered.contains("prune_batch_size"));
}

#[test]
fn continuation_config_rejects_unbounded_operational_values() {
    for (yaml, expected) in [
        (
            "continuation:\n  retention_days: 0\n",
            "continuation.retention_days must be positive",
        ),
        (
            "continuation:\n  prune_batch_size: 0\n",
            "continuation.prune_batch_size must be between 1 and 10000",
        ),
        (
            "continuation:\n  prune_batch_size: 10001\n",
            "continuation.prune_batch_size must be between 1 and 10000",
        ),
    ] {
        let error = parse_with(yaml, |_| None).unwrap_err();
        assert!(error.to_string().contains(expected), "got: {error}");
    }
}

#[test]
fn trajectory_config_is_opt_in_round_trippable_and_schema_visible() {
    let defaults = parse_with("server:\n  listen: 127.0.0.1:4356\n", |_| None).unwrap();
    assert!(!defaults.trajectory.enabled);

    let config = parse_with(
        "trajectory:\n  enabled: true\n  retention_days: 45\n  outbox_batch_size: 250\n",
        |_| None,
    )
    .unwrap();
    assert!(config.trajectory.enabled);
    assert_eq!(config.trajectory.retention_days, 45);
    assert_eq!(config.trajectory.outbox_batch_size, 250);

    let encoded = serde_saphyr::to_string(&config.trajectory).unwrap();
    let decoded: TrajectoryConfig = serde_saphyr::from_str(&encoded).unwrap();
    assert_eq!(decoded, config.trajectory);

    let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
    let trajectory = &schema["properties"]["trajectory"];
    assert!(!trajectory.is_null());
    let rendered = serde_json::to_string(trajectory).unwrap();
    assert!(rendered.contains("retention_days"));
    assert!(rendered.contains("outbox_batch_size"));
}

#[test]
fn trajectory_config_rejects_zero_and_oversized_operational_bounds() {
    for (yaml, expected) in [
        (
            "trajectory:\n  enabled: true\n  retention_days: 0\n",
            "trajectory.retention_days must be positive",
        ),
        (
            "trajectory:\n  enabled: true\n  outbox_batch_size: 0\n",
            "trajectory.outbox_batch_size must be between 1 and 1000",
        ),
        (
            "trajectory:\n  enabled: true\n  outbox_batch_size: 1001\n",
            "trajectory.outbox_batch_size must be between 1 and 1000",
        ),
    ] {
        let error = parse_with(yaml, |_| None).unwrap_err();
        assert!(error.to_string().contains(expected), "got: {error}");
    }
}

#[test]
fn env_substitution_replaces_vars() {
    let out = substitute_with("api_key: ${BR_TEST_KEY}", |n| {
        (n == "BR_TEST_KEY").then(|| "secret-123".to_string())
    })
    .unwrap();
    assert_eq!(out, "api_key: secret-123");
}

#[test]
fn env_substitution_errors_on_undefined() {
    let err = substitute_with("k: ${MISSING}", |_| None).unwrap_err();
    assert_eq!(err.status(), 400);
}

#[test]
fn env_substitution_uses_default_when_var_unset() {
    // `${VAR:-default}` falls back to the default when the var is unset...
    let out = substitute_with(
        "https://bedrock-mantle.${AWS_REGION:-us-east-1}.api.aws/v1",
        |_| None,
    )
    .unwrap();
    assert_eq!(out, "https://bedrock-mantle.us-east-1.api.aws/v1");
    // ...but the set value still wins over the default.
    let out = substitute_with("${AWS_REGION:-us-east-1}", |n| {
        (n == "AWS_REGION").then(|| "eu-west-1".to_string())
    })
    .unwrap();
    assert_eq!(out, "eu-west-1");
    // A default containing hyphens is preserved (split is on the first `:-`).
    let out = substitute_with("${X:-a-b-c}", |_| None).unwrap();
    assert_eq!(out, "a-b-c");
    // A bare `${VAR}` (no `:-`) still errors when unset.
    assert_eq!(
        substitute_with("${AZURE_OPENAI_RESOURCE}", |_| None)
            .unwrap_err()
            .status(),
        400
    );
}

#[test]
fn env_substitution_handles_multiple_and_literals() {
    let out = substitute_with("a=${A} b=${B} c", |n| Some(format!("<{n}>"))).unwrap();
    assert_eq!(out, "a=<A> b=<B> c");
    assert_eq!(substitute_with("x ${oops", |_| None).unwrap(), "x ${oops");
}

// ===== comment-aware substitution (a `${VAR}` in a YAML comment must NOT be
// expanded, and an unset var referenced only from a comment must NOT error) =====

#[test]
fn env_substitution_skips_full_line_comments() {
    // The reference is in a `#` comment → left literal, no lookup, no error,
    // even though the var is unset.
    let out = substitute_with("# api_key: ${MISSING}\nlisten: x", |_| None).unwrap();
    assert_eq!(out, "# api_key: ${MISSING}\nlisten: x");
}

#[test]
fn env_substitution_skips_indented_comments() {
    // Mirrors the `bro init` starter config: a commented example deep in
    // the file referencing an unset var must not break loading.
    let yaml = "providers:\n  # opencode: { key: \"${OPENCODE_KEY_A}\" }\n  openai: {}";
    let out = substitute_with(yaml, |_| None).unwrap();
    assert_eq!(out, yaml);
}

#[test]
fn env_substitution_still_runs_before_an_inline_comment() {
    // The real value is substituted; the `${X}` in the trailing comment is not.
    let out = substitute_with("api_key: ${REAL}  # fallback ${UNUSED}", |n| {
        (n == "REAL").then(|| "sk-123".to_string())
    })
    .unwrap();
    assert_eq!(out, "api_key: sk-123  # fallback ${UNUSED}");
}

#[test]
fn env_substitution_treats_hash_in_value_as_literal_not_comment() {
    // A `#` that is NOT preceded by whitespace (e.g. a URL fragment) is part of
    // the value, so a following `${VAR}` is still substituted.
    let out = substitute_with("url: https://h/p#x=${TOK}", |n| {
        (n == "TOK").then(|| "t".to_string())
    })
    .unwrap();
    assert_eq!(out, "url: https://h/p#x=t");
}

#[test]
fn env_substitution_handles_crlf_line_endings() {
    // Windows CRLF: real values on either side of a commented line are still
    // substituted, and the commented `${C}` is skipped — i.e. comment/quote
    // state resets across the line break, not just a bare `\n`.
    let out =
        substitute_with("a: ${V}\r\n# c ${C}\r\nb: ${W}", |n| Some(format!("<{n}>"))).unwrap();
    assert_eq!(out, "a: <V>\r\n# c ${C}\r\nb: <W>");
}

#[test]
fn env_substitution_inside_quotes_ignores_hash() {
    // A `#` preceded by a space but *inside* a quoted scalar is literal, so the
    // `${VAR}` after it is still substituted.
    let out = substitute_with("note: \"a # b ${V}\"", |n| {
        (n == "V").then(|| "z".to_string())
    })
    .unwrap();
    assert_eq!(out, "note: \"a # b z\"");
}

#[test]
fn env_substitution_in_comment_never_calls_lookup() {
    // Stronger than "no error": the lookup must not even be consulted for a
    // commented reference (so validate-style callers don't record it as missing).
    // `substitute_with` takes `Fn`, so interior mutability is needed to record.
    let seen = std::cell::RefCell::new(Vec::<String>::new());
    let out = substitute_with("k: ${REAL} # ${COMMENTED}", |n| {
        seen.borrow_mut().push(n.to_string());
        Some(format!("<{n}>"))
    })
    .unwrap();
    assert_eq!(out, "k: <REAL> # ${COMMENTED}");
    assert_eq!(seen.into_inner(), vec!["REAL".to_string()]);
}

#[test]
fn parses_registry_style_provider() {
    let yaml = r#"
server:
  listen: "127.0.0.1:9000"
  skip_auth: true
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: ${BR_CFG_KEY}
    api_protocol:
      - "*": chat_completions
      - "gpt-5*": responses
    rate_limits:
      - "*": { requests_per_minute: 60 }
      - "gpt-5*": { requests_per_minute: 10 }
    models:
      - id: gpt-5
      - id: gpt-4o
      - id: o3
        api_protocol: responses
    tags: [paid]
"#;
    let cfg = parse_with(yaml, |n| (n == "BR_CFG_KEY").then(|| "k-abc".to_string())).unwrap();
    assert_eq!(cfg.server.listen, "127.0.0.1:9000");
    assert!(cfg.server.skip_auth);

    let openai = cfg.providers.get("openai").unwrap();
    assert_eq!(openai.api_key, "k-abc");

    // glob-prefix precedence: `gpt-5*` pattern beats `*`
    assert_eq!(openai.protocol_for("gpt-5"), ApiProtocol::Responses);
    assert_eq!(openai.protocol_for("gpt-4o"), ApiProtocol::ChatCompletions);
    // per-model override beats the pattern
    assert_eq!(openai.protocol_for("o3"), ApiProtocol::Responses);
}

#[test]
fn multi_protocol_provider_resolves_set_and_endpoints() {
    // Case 1: one provider advertising several protocols for its models, with a
    // per-protocol endpoint override for the Anthropic Messages path.
    let yaml = r#"
providers:
  minimax:
    api_base: https://api.minimax.io
    api_key: k
    api_protocol:
      - "*": [chat_completions, responses, messages]
    protocol_endpoints:
      messages: https://api.minimax.io/anthropic/v1
    models:
      - id: MiniMax-M2
      - id: special
        api_protocol: responses
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let p = cfg.providers.get("minimax").unwrap();
    // The full ordered protocol set for a pattern-matched model.
    assert_eq!(
        p.protocols_for("MiniMax-M2"),
        vec![
            ApiProtocol::ChatCompletions,
            ApiProtocol::Responses,
            ApiProtocol::Messages
        ]
    );
    // `protocol_for` is the head (preferred default).
    assert_eq!(p.protocol_for("MiniMax-M2"), ApiProtocol::ChatCompletions);
    // A per-model override wins and is a one-element set.
    assert_eq!(p.protocols_for("special"), vec![ApiProtocol::Responses]);
    // Per-protocol endpoint override is keyed by protocol name.
    assert_eq!(
        p.endpoint_for(&ApiProtocol::Messages),
        Some("https://api.minimax.io/anthropic/v1")
    );
    assert_eq!(p.endpoint_for(&ApiProtocol::ChatCompletions), None);
}

#[test]
fn single_protocol_string_still_parses_as_one_element_set() {
    // Backward-compat: a bare protocol string is a one-element set, and
    // `protocol_for` behaves exactly as before.
    let yaml = r#"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k
    api_protocol:
      - "*": chat_completions
    models: [{ id: gpt-4o }]
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let p = cfg.providers.get("openai").unwrap();
    assert_eq!(
        p.protocols_for("gpt-4o"),
        vec![ApiProtocol::ChatCompletions]
    );
    assert_eq!(p.protocol_for("gpt-4o"), ApiProtocol::ChatCompletions);
}

#[test]
fn parses_context_tier_pricing() {
    let yaml = r#"
providers:
  alibaba:
    api_base: https://example.test/v1
    api_key: k
    models:
      - id: qwen-max
        pricing:
          input_micro_usd_per_token: 1.3
          output_micro_usd_per_token: 7.8
          context_tiers:
            - above_input_tokens: 128000
              input_micro_usd_per_token: 2.0
              output_micro_usd_per_token: 12.0
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let model = &cfg.providers.get("alibaba").unwrap().models[0];
    let pricing = model.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_micro_usd_per_token, Some(1.3));
    assert_eq!(pricing.context_tiers.len(), 1);
    assert_eq!(pricing.context_tiers[0].above_input_tokens, 128_000);
    assert_eq!(
        pricing.context_tiers[0].output_micro_usd_per_token,
        Some(12.0)
    );
}

#[test]
fn pricing_without_tiers_parses_flat() {
    // Flat pricing has no context-dependent tiers.
    let yaml = r#"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k
    models:
      - id: gpt-5
        pricing:
          input_micro_usd_per_token: 1.25
          output_micro_usd_per_token: 10.0
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let pricing = cfg.providers.get("openai").unwrap().models[0]
        .pricing
        .as_ref()
        .expect("pricing present");
    assert!(pricing.context_tiers.is_empty());
}

#[test]
fn pricing_preserves_missing_rates_and_cache_rates() {
    let yaml = r#"
providers:
  anthropic:
    api_base: https://api.anthropic.com/v1
    api_key: k
    models:
      - id: claude-test
        pricing:
          input_micro_usd_per_token: 3.0
          cache_read_micro_usd_per_token: 0.3
          cache_write_micro_usd_per_token: 3.75
          context_tiers:
            - above_input_tokens: 128000
              output_micro_usd_per_token: 20.0
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let pricing = cfg.providers.get("anthropic").unwrap().models[0]
        .pricing
        .as_ref()
        .expect("pricing present");
    assert_eq!(pricing.input_micro_usd_per_token, Some(3.0));
    assert_eq!(pricing.cache_read_micro_usd_per_token, Some(0.3));
    assert_eq!(pricing.cache_write_micro_usd_per_token, Some(3.75));
    assert_eq!(pricing.output_micro_usd_per_token, None);
    assert_eq!(
        pricing.context_tiers[0].output_micro_usd_per_token,
        Some(20.0)
    );
    assert_eq!(
        pricing.context_tiers[0].cache_read_micro_usd_per_token,
        None
    );
}

#[test]
fn derives_inherits_empty_fields_from_parent_provider() {
    let yaml = r#"
providers:
  base-openai:
    api_base: https://api.openai.com/v1
    api_key: parent
    api_protocol:
      - "*": chat_completions
      - "gpt-5*": responses
    rate_limits:
      - "*": { rpm: 1000 }
    models:
      - id: gpt-5
      - id: gpt-4o
    tags: [premium]
    auto_discover: true
  azure-prod:
    derives: base-openai
    api_base: https://my-azure.openai.azure.com/v1
    api_key: azure-secret
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let azure = cfg.providers.get("azure-prod").unwrap();
    // Inherited from the parent because azure-prod didn't set them.
    assert_eq!(azure.models.len(), 2);
    assert_eq!(azure.tags, vec!["premium"]);
    assert!(azure.auto_discover);
    // NOT inherited: api_base / api_key are intrinsic.
    assert_eq!(azure.api_base, "https://my-azure.openai.azure.com/v1");
    assert_eq!(azure.api_key, "azure-secret");
    // The `derives` link is cleared so re-resolution is idempotent.
    assert_eq!(azure.derives, None);
}

#[test]
fn child_fields_win_over_parent_in_derives() {
    let yaml = r#"
providers:
  parent:
    api_base: https://parent.example.com
    api_key: p
    models:
      - id: parent-model
  child:
    derives: parent
    api_base: https://child.example.com
    api_key: c
    models:
      - id: child-model
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let child = cfg.providers.get("child").unwrap();
    // The child set its own `models`, so it must NOT inherit the parent's.
    assert_eq!(child.models.len(), 1);
    assert_eq!(child.models[0].id, "child-model");
}

#[test]
fn derives_chain_cycle_is_a_400() {
    let yaml = r#"
providers:
  a:
    api_base: https://a.example.com
    api_key: a
    derives: b
  b:
    api_base: https://b.example.com
    api_key: b
    derives: a
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.to_string().contains("cycle"));
}

#[test]
fn derives_from_unknown_provider_is_a_400() {
    let yaml = r#"
providers:
  child:
    api_base: https://child.example.com
    api_key: c
    derives: does-not-exist
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.to_string().contains("does-not-exist"));
}

#[test]
fn account_api_base_pointing_at_metadata_is_rejected() {
    // A per-account `api_base` override reaches the executor exactly like the
    // provider-level one, so it must face the same SSRF gate — otherwise an
    // `accounts` entry is an unchecked back door to the host's network.
    let yaml = r#"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k
    accounts:
      - api_key: k2
        api_base: http://169.254.169.254/
        label: rogue
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert_eq!(err.status(), 400);
    let msg = err.to_string();
    assert!(msg.contains("account 'rogue'"), "got: {msg}");
    assert!(msg.contains("api_base rejected"), "got: {msg}");
}

#[test]
fn valid_account_api_base_override_is_accepted() {
    // An https override is fine; an empty override inherits the provider base.
    let yaml = r#"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k
    accounts:
      - api_key: k2
        api_base: https://eu.api.openai.com/v1
      - api_key: k3
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let accounts = &cfg.providers["openai"].accounts;
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].api_base, "https://eu.api.openai.com/v1");
    assert!(accounts[1].api_base.is_empty());
}

#[test]
fn parses_strict_named_router_configuration() -> crate::gateway::error::Result<()> {
    let config = parse(
        r#"
routers:
  project-coding:
    selection:
      kind: model
      model: vendor:base
      routing:
        sort: cost
        only: [vendor]
    defaults:
      system_prompt: Be precise.
      params:
        temperature: 0.2
  adaptive:
    selection:
      kind: policy
      policy: coding
      base_model: vendor:strong
"#,
    )?;

    assert_eq!(config.routers.len(), 2);
    let project = config
        .routers
        .get("project-coding")
        .ok_or_else(|| BitrouterError::internal("project router was not parsed"))?;
    assert_eq!(
        project.defaults.system_prompt.as_deref(),
        Some("Be precise.")
    );
    assert_eq!(project.defaults.params["temperature"], 0.2);
    match &project.selection {
        router::RouterSelection::Model { model, routing } => {
            assert_eq!(model, "vendor:base");
            assert_eq!(routing.sort, Some(bitrouter_ai::routing::SortOrder::Cost));
            assert_eq!(routing.only, vec!["vendor"]);
        }
        router::RouterSelection::Policy { .. } => {
            return Err(BitrouterError::internal(
                "model router parsed as policy selection",
            ));
        }
    }
    Ok(())
}

#[test]
fn named_router_configuration_rejects_unknown_nested_fields() {
    for yaml in [
        r#"
routers:
  project:
    selection: { kind: model, model: vendor:base, checks: [] }
"#,
        r#"
routers:
  project:
    selection:
      kind: model
      model: vendor:base
      routing: { only: [vendor], typo: true }
"#,
        r#"
routers:
  project:
    selection: { kind: model, model: vendor:base }
    defaults: { params: {}, typo: true }
"#,
        r#"
routers:
  project:
    selection: { kind: model, model: vendor:base }
    workflow: later
"#,
    ] {
        assert!(parse(yaml).is_err(), "unknown router field was accepted");
    }
}

#[test]
fn default_config_does_not_implicitly_create_coding_router() {
    assert!(Config::default().routers.is_empty());
}

#[test]
fn auto_router_template_resolves_auto_and_explicit_cost_router()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../templates/auto-router/bitrouter.yaml");
    let mut config = parse(&std::fs::read_to_string(path)?)?;
    let auto = config.resolve_router("bitrouter/auto")?;
    assert_eq!(auto.clean_model, "openai-codex:gpt-5.6-sol");
    assert_eq!(auto.policy.as_deref(), Some("auto"));
    assert!(config.resolve_router("bitrouter/auto:cost").is_err());
    let mut cost_router = config
        .routers
        .get("auto")
        .ok_or("template auto router missing")?
        .clone();
    match &mut cost_router.selection {
        super::router::RouterSelection::Policy { routing, .. } => {
            routing.sort = Some(SortOrder::Cost);
        }
        _ => return Err("template auto router must select a Policy".into()),
    }
    config.routers.insert("auto-cost".into(), cost_router);
    let cost = config.resolve_router("bitrouter/auto-cost")?;
    assert_eq!(cost.clean_model, auto.clean_model);
    assert_eq!(cost.policy, auto.policy);
    assert_eq!(cost.prefs.sort, SortOrder::Cost);
    let physical = config.resolve_router("openai-codex:gpt-5.6-sol")?;
    assert!(physical.policy.is_none());
    Ok(())
}

#[test]
fn policy_source_location_is_strict_and_modes_are_not_accepted() -> crate::gateway::error::Result<()>
{
    let config = parse("policy: {path: ./team.yaml}")?;
    assert_eq!(
        config.policy.path.as_deref(),
        Some(std::path::Path::new("./team.yaml"))
    );
    for source in [
        "policy: {mode: frozen}",
        "policy: {mode: adaptive}",
        "policy: {writeback: locked}",
        "policy: {writeback: evolve}",
        "policy: {unknown: value}",
        "presets: {}",
        "variants: {}",
    ] {
        assert!(parse(source).is_err(), "{source}");
    }
    Ok(())
}

#[test]
fn parses_multi_account_provider() {
    let yaml = r#"
providers:
  opencode-go:
    api_base: https://opencode.ai/zen/go/v1
    account_strategy: balance
    accounts:
      - { api_key: key-a, label: sub-a }
      - { api_key: key-b }
"#;
    let cfg = parse(yaml).unwrap();
    let p = &cfg.providers["opencode-go"];
    assert_eq!(p.accounts.len(), 2);
    assert_eq!(p.account_strategy, AccountStrategy::Balance);
    assert_eq!(p.accounts[0].api_key, "key-a");
    assert_eq!(p.accounts[0].label, "sub-a");
    assert!(p.accounts[1].label.is_empty());
}

#[test]
fn account_strategy_defaults_to_failover() {
    // A provider with `accounts:` and no explicit strategy.
    let cfg = parse("providers:\n  p:\n    accounts:\n      - { api_key: k }\n").unwrap();
    assert_eq!(
        cfg.providers["p"].account_strategy,
        AccountStrategy::Failover
    );
}

#[test]
fn parses_virtual_model_strategy_priority_and_cascade() {
    // Both spellings the audited config documented must still parse — the
    // field is now a typed enum but the YAML surface is unchanged.
    let yaml = r#"
providers:
  a:
    api_base: https://a.example/v1
    api_key: k
    models: [{ id: m }]
models:
  fast:
    strategy: priority
    endpoints:
      - { provider: a, service_id: m }
  cheap:
    strategy: cascade
    endpoints:
      - { provider: a, service_id: m }
"#;
    let cfg = parse(yaml).unwrap();
    assert_eq!(cfg.models["fast"].strategy, VirtualModelStrategy::Priority);
    assert_eq!(cfg.models["cheap"].strategy, VirtualModelStrategy::Cascade);
}

#[test]
fn virtual_model_strategy_defaults_to_priority() {
    // A virtual model with no explicit `strategy:` keeps declared order.
    let yaml = r#"
providers:
  a:
    api_base: https://a.example/v1
    api_key: k
    models: [{ id: m }]
models:
  v:
    endpoints:
      - { provider: a, service_id: m }
"#;
    let cfg = parse(yaml).unwrap();
    assert_eq!(cfg.models["v"].strategy, VirtualModelStrategy::Priority);
}

#[test]
fn primary_api_key_prefers_top_level_then_first_account() {
    // Top-level key wins when set.
    let mut p = ProviderConfig {
        api_key: "top".to_string(),
        ..ProviderConfig::default()
    };
    p.accounts = vec![ProviderAccount {
        api_key: "acct".to_string(),
        ..ProviderAccount::default()
    }];
    assert_eq!(p.primary_api_key(), "top");

    // Falls back to the first non-empty account key when there's no
    // top-level key — the account-managed case (used by model discovery).
    let p2 = ProviderConfig {
        accounts: vec![
            ProviderAccount::default(),
            ProviderAccount {
                api_key: "second".to_string(),
                ..ProviderAccount::default()
            },
        ],
        ..ProviderConfig::default()
    };
    assert_eq!(p2.primary_api_key(), "second");
}

#[test]
fn undeclared_config_sections_use_unknown_field_errors() -> crate::gateway::error::Result<()> {
    for field in ["policy_table", "agent_api", "chat", "unknown_setting"] {
        let error = parse(&format!("{field}: {{}}\n"))
            .err()
            .ok_or_else(|| BitrouterError::internal("unknown field accepted"))?;
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "{error}"
        );
    }
    Ok(())
}

// ===== upstream HTTP timeouts (global + per-provider) =====

#[test]
fn timeout_config_empty_inherits_base() {
    use crate::gateway::model::client::HttpTimeouts;
    let base = HttpTimeouts::default();
    let resolved = TimeoutConfig::default().apply_to(base.clone());
    assert_eq!(resolved, base, "an empty override must equal the base");
}

#[test]
fn timeout_config_overrides_only_set_fields() {
    use crate::gateway::model::client::HttpTimeouts;
    use std::time::Duration;
    let base = HttpTimeouts::default();
    let resolved = TimeoutConfig {
        read_secs: Some(300),
        ..Default::default()
    }
    .apply_to(base.clone());
    assert_eq!(resolved.read, Duration::from_secs(300));
    // Untouched fields keep the base value.
    assert_eq!(resolved.connect, base.connect);
    assert_eq!(resolved.pool_idle, base.pool_idle);
    assert_eq!(resolved.tcp_keepalive, base.tcp_keepalive);
}

#[test]
fn total_wall_clock_cap_is_off_by_default_and_opt_in() {
    use crate::gateway::model::client::HttpTimeouts;
    use std::time::Duration;
    // No overall cap by default — correct for long agentic streams.
    assert_eq!(HttpTimeouts::default().total, None);
    // Opt-in via config.
    let resolved = TimeoutConfig {
        total_secs: Some(900),
        ..Default::default()
    }
    .apply_to(HttpTimeouts::default());
    assert_eq!(resolved.total, Some(Duration::from_secs(900)));
}

#[test]
fn per_provider_override_layers_over_resolved_global() {
    use crate::gateway::model::client::HttpTimeouts;
    use std::time::Duration;
    // A provider inherits the global read but sets its own total cap.
    let global = TimeoutConfig {
        read_secs: Some(200),
        ..Default::default()
    }
    .apply_to(HttpTimeouts::default());
    let provider = TimeoutConfig {
        total_secs: Some(600),
        ..Default::default()
    }
    .apply_to(global.clone());
    assert_eq!(
        provider.read,
        Duration::from_secs(200),
        "inherits global read"
    );
    assert_eq!(
        provider.total,
        Some(Duration::from_secs(600)),
        "own total cap"
    );
}

#[test]
fn parses_global_and_per_provider_timeouts() {
    let yaml = r#"
upstream:
  timeouts:
    read_secs: 200
    total_secs: 600
providers:
  slow:
    api_base: https://api.example.com
    api_key: k
    timeouts:
      read_secs: 300
"#;
    let cfg = parse(yaml).expect("parse");
    assert_eq!(cfg.upstream.timeouts.read_secs, Some(200));
    assert_eq!(cfg.upstream.timeouts.total_secs, Some(600));
    let p = cfg.providers.get("slow").expect("provider 'slow'");
    assert_eq!(p.timeouts.read_secs, Some(300));
    assert_eq!(
        p.timeouts.total_secs, None,
        "unset provider field stays None"
    );
}

#[test]
fn parses_opt_in_fallback_backoff_schedule() {
    let cfg = parse(
        r#"
upstream:
  fallback_backoff_ms: [1000, 2000, 4000, 8000]
"#,
    )
    .expect("parse");

    assert_eq!(
        cfg.upstream.fallback_backoff_ms,
        vec![1000, 2000, 4000, 8000]
    );
    assert!(
        Config::default().upstream.fallback_backoff_ms.is_empty(),
        "existing deployments must retain immediate fallback by default"
    );
}

#[test]
fn mcp_upstream_protocol_defaults_to_auto() {
    // Absent `mcp:` and present-but-silent `mcp:` must agree, and both must
    // prefer modern discovery with the compatibility fallback.
    assert_eq!(
        Config::default().mcp.upstream_protocol,
        McpUpstreamProtocol::Auto
    );
    let cfg = parse("mcp:\n  cache:\n    enabled: true\n").expect("parse");
    assert_eq!(cfg.mcp.upstream_protocol, McpUpstreamProtocol::Auto);
}

#[test]
fn mcp_upstream_protocol_opts_in_by_version_string() {
    let auto = parse("mcp:\n  upstream_protocol: auto\n").expect("parse auto");
    assert_eq!(auto.mcp.upstream_protocol, McpUpstreamProtocol::Auto);
    let latest = parse("mcp:\n  upstream_protocol: latest\n").expect("parse latest");
    assert_eq!(latest.mcp.upstream_protocol, McpUpstreamProtocol::Latest);

    let cfg = parse("mcp:\n  upstream_protocol: \"2026-07-28\"\n").expect("parse");
    assert_eq!(cfg.mcp.upstream_protocol, McpUpstreamProtocol::V2026_07_28);

    // The version is spelled as itself, so an unknown one is a config error
    // rather than a silent downgrade to the default.
    assert!(
        parse("mcp:\n  upstream_protocol: \"2099-01-01\"\n").is_err(),
        "unknown protocol version must not parse"
    );
}

#[test]
fn rejects_local_harness_configuration_as_unknown() {
    for field in ["agent_api", "chat"] {
        let error = parse(&format!("{field}: {{}}\n")).expect_err("unknown field");
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "{error}"
        );
    }
}

#[test]
fn native_checker_config_rejects_mixed_transports_and_tracks_revision()
-> crate::gateway::error::Result<()> {
    let source = |revision: &str| {
        format!(
            r#"
inherit_defaults: false
checkers:
  safety:
    native:
      revision: {revision}
routers:
  guarded:
    selection:
      kind: model
      model: vendor:model
    checks:
      request:
        - checker: safety
"#
        )
    };
    let first = parse_with(&source("rules-v1"), |_| None)?;
    let second = parse_with(&source("rules-v2"), |_| None)?;
    let first = first.resolve_router("bitrouter/guarded")?;
    let second = second.resolve_router("bitrouter/guarded")?;
    assert_ne!(first.router, second.router);
    assert_ne!(first.request_checks, second.request_checks);
    for fields in [
        "native: {revision: ''}",
        "native: {revision: 'contains spaces'}",
        "native: {revision: v1, typo: true}",
        "native: {revision: v1}\n    endpoint: https://example.com/check\n    contract_version: 1",
        "native: {revision: v1}\n    credential_env: TOKEN",
    ] {
        assert!(
            parse_with(
                &format!("inherit_defaults: false\ncheckers:\n  safety:\n    {fields}"),
                |_| None
            )
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn retired_active_protocols_and_providers_require_explicit_migration()
-> crate::gateway::error::Result<()> {
    for (definition, location) in [
        (
            "api_protocol: [{'*': [chat_completions, generate_content]}]",
            "providers.test.api_protocol",
        ),
        (
            "models: [{id: m, api_protocol: antigravity}]",
            "providers.test.models[0].api_protocol",
        ),
        (
            "api_base: https://generativelanguage.googleapis.com/v1beta",
            "providers.test.api_base",
        ),
        (
            "api_base: https://generativelanguage.googleapis.com/v1beta\n    models: [{id: migrated, api_protocol: chat_completions}, {id: still-native}]",
            "providers.test.api_base",
        ),
    ] {
        let yaml = format!(
            "inherit_defaults: false\nproviders:\n  test:\n    active: true\n    {definition}\n"
        );
        let error = parse_with(&yaml, |_| None)
            .err()
            .ok_or_else(|| BitrouterError::internal("retired config accepted"))?;
        assert!(error.to_string().contains(location), "{error}");
    }
    for retired in ["google-ai", "vertex"] {
        let yaml = format!(
            "inherit_defaults: false\nproviders:\n  {retired}:\n    active: true\n    api_protocol: [{{'*': chat_completions}}]\n"
        );
        let error = parse_with(&yaml, |_| None)
            .err()
            .ok_or_else(|| BitrouterError::internal("retired provider accepted"))?;
        assert!(error.to_string().contains("retired"));
        let config = parse_with("inherit_defaults: false", |_| None)?;
        assert!(
            config
                .resolve_router(&format!("{retired}:google/fixture"))
                .is_err()
        );
    }
    assert_eq!(
        infer_protocol("https://generativelanguage.googleapis.com/v1beta/openai"),
        ApiProtocol::ChatCompletions
    );
    assert_eq!(
        infer_protocol("https://aiplatform.googleapis.com/v1/publishers/google"),
        ApiProtocol::Custom("generate_content".into())
    );
    Ok(())
}
