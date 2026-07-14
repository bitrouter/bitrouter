//! Config parsing + `${VAR}` substitution tests.

use super::*;
use crate::language_model::types::ApiProtocol;

#[test]
fn defaults_are_sane() {
    let cfg = Config::default();
    assert_eq!(cfg.server.listen, "0.0.0.0:4356");
    assert!(
        !cfg.server.skip_auth,
        "skip_auth code default must be false"
    );
    assert!(cfg.inherit_defaults);
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
    // Mirrors the `bitrouter init` starter config: a commented example deep in
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
    assert_eq!(pricing.input_micro_usd_per_token, 1.3);
    assert_eq!(pricing.context_tiers.len(), 1);
    assert_eq!(pricing.context_tiers[0].above_input_tokens, 128_000);
    assert_eq!(pricing.context_tiers[0].output_micro_usd_per_token, 12.0);
}

#[test]
fn pricing_without_tiers_parses_flat() {
    // Back-compat: a model priced the old way has no tiers.
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
fn protocol_inference_from_api_base() {
    assert_eq!(
        infer_protocol("https://api.anthropic.com/v1"),
        ApiProtocol::Messages
    );
    assert_eq!(
        infer_protocol("https://generativelanguage.googleapis.com/v1beta"),
        ApiProtocol::GenerateContent
    );
    assert_eq!(
        infer_protocol("https://api.openai.com/v1"),
        ApiProtocol::ChatCompletions
    );
    assert_eq!(
        infer_protocol("https://my-llm.example.com/v1"),
        ApiProtocol::ChatCompletions
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
fn parses_presets_and_variants() {
    let yaml = r#"
presets:
  careful:
    model: gpt-5
    system_prompt: "Reason carefully."
    params: { temperature: 0.2 }
    routing: { require_tags: [paid], sort: latency }
variants:
  free:
    routing: { require_tags: [free] }
"#;
    let cfg = parse(yaml).unwrap();
    let careful = cfg.presets.get("careful").unwrap();
    assert_eq!(careful.model.as_deref(), Some("gpt-5"));
    assert_eq!(careful.routing.require_tags, vec!["paid"]);
    assert!(cfg.variants.contains_key("free"));
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
fn policy_table_absent_leaves_section_empty() {
    // No `policy_table:` block → the section defaults to inert (no tiers).
    let cfg = parse_with(
        "providers:\n  a:\n    api_base: https://a.example/v1\n",
        |_| None,
    )
    .unwrap();
    assert!(cfg.policy_table.tiers.is_empty());
    assert!(cfg.policy_table.fingerprints.is_empty());
    assert_eq!(
        cfg.policy_table.key_strategy,
        PolicyKeyStrategy::LegacyFingerprint
    );
    assert!(cfg.policy_table.default_tier.is_none());
    assert!(cfg.policy_table.tool_use_tier.is_none());
    assert!(cfg.policy_table.tool_safe_tiers.is_empty());
}

#[test]
fn parses_policy_table_workflow_state_key_strategy() {
    let yaml = r#"
policy_table:
  key_strategy: workflow_state
  tiers:
    cheap: vendor/cheap
  fingerprints:
    "generic|unknown|opening|-|-|-|none|small|none|low|low|low|low|medium|medium|": cheap
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    assert_eq!(
        cfg.policy_table.key_strategy,
        PolicyKeyStrategy::WorkflowState
    );
}

#[test]
fn parses_policy_table_section() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
    flagship: vendor/flagship
  fingerprints:
    opening: flagship
    after_read_file: cheap
  default_tier: flagship
  tool_use_tier: flagship
  tool_safe_tiers:
    - flagship
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    assert_eq!(
        cfg.policy_table.tiers.get("cheap").map(String::as_str),
        Some("vendor/cheap")
    );
    assert_eq!(
        cfg.policy_table
            .fingerprints
            .get("opening")
            .map(String::as_str),
        Some("flagship")
    );
    assert_eq!(cfg.policy_table.default_tier.as_deref(), Some("flagship"));
    assert_eq!(cfg.policy_table.tool_use_tier.as_deref(), Some("flagship"));
    assert_eq!(
        cfg.policy_table.tool_safe_tiers,
        vec!["flagship".to_string()]
    );
}

#[test]
fn policy_table_unknown_tier_is_a_400() {
    // A fingerprint that maps to a tier absent from `tiers:` is a config error,
    // not a silent fall-through.
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
  fingerprints:
    opening: flagship
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string().contains("unknown tier 'flagship'"),
        "got: {err}"
    );
}

#[test]
fn policy_table_tool_use_tier_must_be_tool_safe() {
    // The guardrail target must itself be declared tool-safe, else the floor it
    // clamps tool requests to is not actually safe.
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
    capable: vendor/capable
  tool_use_tier: capable
  tool_safe_tiers:
    - cheap
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string()
            .contains("tool_use_tier 'capable' must also be listed in tool_safe_tiers"),
        "got: {err}"
    );
}

#[test]
fn parses_adequacy_section() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
    capable: vendor/capable
  default_tier: capable
  adequacy:
    enabled: true
    escalation_tier: capable
    escalation_threshold: 3
    pin_cooldown_secs: 600
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let adequacy = &cfg.policy_table.adequacy;
    assert!(adequacy.enabled);
    assert_eq!(adequacy.escalation_tier.as_deref(), Some("capable"));
    assert_eq!(adequacy.escalation_threshold, 3);
    assert_eq!(adequacy.pin_cooldown_secs, 600);
}

#[test]
fn adequacy_defaults_when_section_omitted() {
    // A `policy_table:` with no `adequacy:` block is off, with sane defaults.
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let adequacy = &cfg.policy_table.adequacy;
    assert!(!adequacy.enabled);
    assert_eq!(adequacy.escalation_threshold, 1);
    assert_eq!(adequacy.pin_cooldown_secs, 1800);
    assert!(!adequacy.explore_opening);
    assert_eq!(adequacy.min_semantic_successes_for_lock, 0);
    assert_eq!(adequacy.min_semantic_successes_for_opening, 1);
}

#[test]
fn adequacy_unknown_escalation_tier_is_a_400() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
  adequacy:
    enabled: true
    escalation_tier: capable
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string()
            .contains("adequacy.escalation_tier references unknown tier 'capable'"),
        "got: {err}"
    );
}

#[test]
fn adequacy_enabled_without_escalation_target_is_a_400() {
    // Enabled, but neither escalation_tier nor default_tier is set — a pin would
    // have nowhere to escalate to.
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
  adequacy:
    enabled: true
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string().contains("no escalation target is set"),
        "got: {err}"
    );
}

#[test]
fn parses_exploration_section() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
    capable: vendor/capable
  default_tier: capable
  adequacy:
    enabled: true
    explore_enabled: true
    explore_tier: cheap
    explore_interval: 8
    explore_threshold: 4
    min_semantic_successes_for_lock: 3
    explore_opening: true
    min_semantic_successes_for_opening: 2
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let adequacy = &cfg.policy_table.adequacy;
    assert!(adequacy.explore_enabled);
    assert_eq!(adequacy.explore_tier.as_deref(), Some("cheap"));
    assert_eq!(adequacy.explore_interval, 8);
    assert_eq!(adequacy.explore_threshold, 4);
    assert_eq!(adequacy.min_semantic_successes_for_lock, 3);
    assert!(adequacy.explore_opening);
    assert_eq!(adequacy.min_semantic_successes_for_opening, 2);
}

#[test]
fn exploration_defaults_when_omitted() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
"#;
    let cfg = parse_with(yaml, |_| None).unwrap();
    let adequacy = &cfg.policy_table.adequacy;
    assert!(!adequacy.explore_enabled);
    assert_eq!(adequacy.explore_interval, 5);
    assert_eq!(adequacy.explore_threshold, 3);
}

#[test]
fn exploration_unknown_explore_tier_is_a_400() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
  default_tier: cheap
  adequacy:
    enabled: true
    explore_tier: nope
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string()
            .contains("adequacy.explore_tier references unknown tier 'nope'"),
        "got: {err}"
    );
}

#[test]
fn exploration_enabled_without_target_is_a_400() {
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
  default_tier: cheap
  adequacy:
    enabled: true
    explore_enabled: true
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string()
            .contains("explore_enabled is set but adequacy.explore_tier is not"),
        "got: {err}"
    );
}

#[test]
fn exploration_requires_adequacy_enabled() {
    // `explore_enabled` without `enabled` would be silently inert (the ledger is
    // only wired when learning is on) — reject it loudly.
    let yaml = r#"
policy_table:
  tiers:
    cheap: vendor/cheap
  default_tier: cheap
  adequacy:
    explore_enabled: true
    explore_tier: cheap
"#;
    let err = parse_with(yaml, |_| None).unwrap_err();
    assert!(
        err.to_string()
            .contains("explore_enabled requires adequacy.enabled"),
        "got: {err}"
    );
}

// ===== upstream HTTP timeouts (global + per-provider) =====

#[test]
fn timeout_config_empty_inherits_base() {
    use crate::language_model::HttpTimeouts;
    let base = HttpTimeouts::default();
    let resolved = TimeoutConfig::default().apply_to(base.clone());
    assert_eq!(resolved, base, "an empty override must equal the base");
}

#[test]
fn timeout_config_overrides_only_set_fields() {
    use crate::language_model::HttpTimeouts;
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
    use crate::language_model::HttpTimeouts;
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
    use crate::language_model::HttpTimeouts;
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
