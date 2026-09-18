use std::fs;

use bitrouter_regex_checker::startup;
use tempfile::tempdir;

fn load(source: &str) -> Result<bitrouter_guardrails::rules::RuleSet, Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let path = directory.path().join("rules.yaml");
    fs::write(&path, source)?;
    startup::load_rules(&path).map_err(Into::into)
}

#[test]
fn valid_yaml_activates_fixed_input_block_rules() -> Result<(), Box<dyn std::error::Error>> {
    let rules = load(
        r#"
scope: input
rules:
  - name: credential
    pattern: 'token-[0-9]+'
    action: block
"#,
    )?;
    assert_eq!(rules.first_block("token-42"), Some("credential"));
    Ok(())
}

#[test]
fn startup_rejects_redact_output_unknown_fields_and_missing_action() {
    let invalid = [
        r#"scope: input
rules:
  - name: secret
    pattern: secret
    action: redact
"#,
        r#"scope: output
rules:
  - name: secret
    pattern: secret
    action: block
"#,
        r#"scope: input
rules:
  - name: secret
    pattern: secret
    action: block
    enabled: true
"#,
        r#"scope: input
rules:
  - name: secret
    pattern: secret
"#,
    ];
    for source in invalid {
        assert!(load(source).is_err());
    }
}

#[test]
fn startup_rejects_invalid_regex_without_leaking_rule_data()
-> Result<(), Box<dyn std::error::Error>> {
    let result = load(
        r#"scope: input
rules:
  - name: private-name
    pattern: 'private-secret-('
    action: block
"#,
    );
    let message = result
        .err()
        .ok_or_else(|| std::io::Error::other("invalid regex unexpectedly activated"))?
        .to_string();
    assert!(!message.contains("private-name"));
    assert!(!message.contains("private-secret"));
    Ok(())
}
