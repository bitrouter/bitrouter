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

    // rate limits: `gpt-5*` and `*` are independent buckets
    assert_eq!(
        openai.rate_limit_for("gpt-5").unwrap().requests_per_minute,
        Some(10)
    );
    assert_eq!(
        openai.rate_limit_for("gpt-4o").unwrap().requests_per_minute,
        Some(60)
    );
    let bucket_gpt5 = openai.rate_limit_bucket("openai", "gpt-5").unwrap();
    let bucket_4o = openai.rate_limit_bucket("openai", "gpt-4o").unwrap();
    assert_ne!(
        bucket_gpt5, bucket_4o,
        "per-pattern keyed buckets are distinct"
    );
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
