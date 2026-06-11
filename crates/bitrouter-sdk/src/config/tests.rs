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
fn env_substitution_handles_multiple_and_literals() {
    let out = substitute_with("a=${A} b=${B} c", |n| Some(format!("<{n}>"))).unwrap();
    assert_eq!(out, "a=<A> b=<B> c");
    assert_eq!(substitute_with("x ${oops", |_| None).unwrap(), "x ${oops");
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
