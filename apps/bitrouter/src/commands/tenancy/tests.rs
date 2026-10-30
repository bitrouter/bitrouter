use super::*;
use anyhow::ensure;
use bitrouter::output::{Format, error::envelope_from_anyhow};
use clap::Parser as _;
use serde_json::{Value, json};

#[path = "tests/http.rs"]
mod http;

const TOKEN: &str = "fixture-tenancy-cli-0123456789abcdef";

fn target(endpoint: &str) -> Target {
    Target {
        endpoint: Some(endpoint.into()),
        token_env: Some("FIXTURE_TENANCY_TOKEN".into()),
    }
}

fn draft() -> Value {
    json!({
        "installation_id": "install-A", "operation_id": "prepare-1",
        "expected_authority_revision": 17,
        "document": {
            "routes": [{"owner_kind":"tenant", "owner_id":"Tenant-A", "resource_kind":"route", "resource_id":"coding:Exact", "revision_id":"route-1"}],
            "policy": null,
            "generation_defaults": {"max_output_tokens":2048, "temperature":0.2},
            "learning": {"mode":"off", "max_evaluations":0},
            "limits": {"active_executions":16, "queued_work":32, "learning_jobs":0}
        }
    })
}

fn activation() -> Value {
    json!({"installation_id":"install-A", "operation_id":"activate-1",
        "expected_authority_revision":18, "expected_active_revision":null,
        "revision_id":"configuration-1"})
}

fn parsed(args: &[&str]) -> Result<(Action, Option<String>)> {
    let cli = crate::Cli::try_parse_from(args)?;
    let Some(crate::Command::Tenancy { action }) = cli.command else {
        anyhow::bail!("missing tenancy command");
    };
    Ok((action, cli.context))
}

fn write_json(directory: &Path, name: &str, value: &Value) -> Result<PathBuf> {
    let path = directory.join(name);
    std::fs::write(&path, serde_json::to_vec(value)?)?;
    Ok(path)
}

#[test]
fn tenancy_cli_parses_all_actions_with_explicit_identities() -> Result<()> {
    let leaves = [
        vec!["capabilities"],
        vec![
            "operation",
            "--installation-id",
            "install-A",
            "--operation-id",
            "prepare-1",
        ],
        vec![
            "configuration",
            "apply",
            "--tenant",
            "Tenant-A",
            "--input",
            "request.json",
        ],
        vec![
            "configuration",
            "read",
            "--tenant",
            "Tenant-A",
            "--installation-id",
            "install-A",
            "--revision",
            "configuration-1",
        ],
        vec![
            "configuration",
            "activate",
            "--tenant",
            "Tenant-A",
            "--input",
            "activation.json",
        ],
        vec![
            "configuration",
            "active",
            "--tenant",
            "Tenant-A",
            "--installation-id",
            "install-A",
        ],
    ];
    for leaf in leaves {
        let mut named = vec!["bro", "--context", "fixture", "tenancy"];
        named.extend(&leaf);
        let (_, context) = parsed(&named)?;
        ensure!(context.as_deref() == Some("fixture"));
        let mut explicit = vec!["bro", "tenancy"];
        explicit.extend(&leaf);
        explicit.extend([
            "--endpoint",
            "https://router.example/control/v1",
            "--token-env",
            "FIXTURE_TENANCY_TOKEN",
            "--human",
        ]);
        let (_, context) = parsed(&explicit)?;
        ensure!(context.is_none());
    }
    let (action, _) = parsed(&[
        "bro",
        "tenancy",
        "configuration",
        "validate",
        "--input",
        "request.json",
    ])?;
    ensure!(matches!(
        action,
        Action::Configuration {
            action: ConfigurationAction::Validate { .. }
        }
    ));
    Ok(())
}

#[test]
fn tenancy_cli_rejects_ambiguous_targets_secret_flags_and_missing_read_ids() {
    for args in [
        vec![
            "bro",
            "tenancy",
            "capabilities",
            "--endpoint",
            "https://router.example",
        ],
        vec!["bro", "tenancy", "capabilities", "--token-env", "TOKEN"],
        vec![
            "bro",
            "tenancy",
            "capabilities",
            "--token",
            "SECRET_SENTINEL",
        ],
        vec![
            "bro",
            "tenancy",
            "configuration",
            "validate",
            "--input",
            "request.json",
            "--endpoint",
            "https://router.example",
            "--token-env",
            "TOKEN",
        ],
        vec![
            "bro",
            "--context",
            "fixture",
            "tenancy",
            "configuration",
            "read",
            "--tenant",
            "Tenant-A",
            "--revision",
            "configuration-1",
        ],
        vec![
            "bro",
            "--context",
            "fixture",
            "tenancy",
            "configuration",
            "active",
            "--installation-id",
            "install-A",
        ],
        vec![
            "bro",
            "--context",
            "fixture",
            "tenancy",
            "operation",
            "--installation-id",
            "install-A",
            "--operation-id",
            "../invalid",
        ],
        vec![
            "bro",
            "--context",
            "fixture",
            "tenancy",
            "capabilities",
            "--config",
            "bitrouter.yaml",
        ],
        vec![
            "bro",
            "--context",
            "fixture",
            "tenancy",
            "capabilities",
            "--socket",
            "bitrouter.sock",
        ],
    ] {
        assert!(
            crate::Cli::try_parse_from(&args).is_err(),
            "accepted conflicting arguments: {args:?}"
        );
    }
}

#[tokio::test]
async fn local_validation_does_not_resolve_context_read_tokens_or_claim_server_checks() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let input = write_json(directory.path(), "draft.json", &draft())?;
    let action = Action::Configuration {
        action: ConfigurationAction::Validate { input },
    };
    let lookups = std::cell::Cell::new(0);
    let result = execute(&action, None, |_| {
        lookups.set(lookups.get() + 1);
        None
    })
    .await?;
    let report = serde_json::to_value(&result.report)?;
    ensure!(
        report
            == json!({"valid":true, "validation":"local_shape_only", "contract":"ConfigurationPut", "server_checked":false})
    );
    ensure!(result.recovery.is_none() && lookups.get() == 0);
    let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&result.report))?;
    ensure!(human.contains("local only") && human.contains("No server authorization"));
    for context in ["local", "missing-context"] {
        let error = execute(&action, Some(context), |_| {
            lookups.set(lookups.get() + 1);
            None
        })
        .await
        .err()
        .context("accepted a context during local validation")?;
        ensure!(error.to_string().contains("local shape validation"));
    }
    ensure!(lookups.get() == 0);
    Ok(())
}

#[tokio::test]
async fn typed_inputs_require_explicit_operation_installation_and_cas_fields() -> Result<()> {
    for field in [
        "installation_id",
        "operation_id",
        "expected_authority_revision",
        "document",
    ] {
        let mut input = draft();
        input.as_object_mut().context("draft object")?.remove(field);
        ensure!(
            read_json::<ConfigurationPut>(&serde_json::to_vec(&input)?[..])
                .await
                .is_err()
        );
    }
    for field in [
        "installation_id",
        "operation_id",
        "expected_authority_revision",
        "expected_active_revision",
        "revision_id",
    ] {
        let mut input = activation();
        input
            .as_object_mut()
            .context("activation object")?
            .remove(field);
        ensure!(
            read_json::<Activation>(&serde_json::to_vec(&input)?[..])
                .await
                .is_err()
        );
    }
    let input: Activation = read_json(&serde_json::to_vec(&activation())?[..]).await?;
    ensure!(input.expected_active_revision.is_none());
    let mut rollback = activation();
    rollback["expected_active_revision"] = json!("configuration-2");
    let input: Activation = read_json(&serde_json::to_vec(&rollback)?[..]).await?;
    ensure!(
        input
            .expected_active_revision
            .as_ref()
            .map(RevisionId::as_str)
            == Some("configuration-2")
    );
    Ok(())
}

#[tokio::test]
async fn input_bounds_duplicate_fields_and_untrusted_diagnostics_are_enforced() -> Result<()> {
    let valid = serde_json::to_vec(&draft())?;
    let mut boundary = valid.clone();
    boundary.resize(INPUT_LIMIT as usize, b' ');
    read_json::<ConfigurationPut>(&boundary[..]).await?;
    boundary.push(b' ');
    ensure!(read_json::<ConfigurationPut>(&boundary[..]).await.is_err());
    let text = String::from_utf8(valid.clone())?;
    let duplicate = format!("{{\"operation_id\":\"SECRET_SENTINEL\",{}", &text[1..]);
    let nested = text.replace(
        "\"temperature\":0.2",
        "\"temperature\":0.2,\"temperature\":\"SECRET_SENTINEL\"",
    );
    let mut unknown = draft();
    unknown["document"]["api_key"] = json!("SECRET_SENTINEL");
    let mut literal = draft();
    literal["installation_id"] = json!("${SECRET_SENTINEL}");
    let mut numeric = draft();
    numeric["expected_authority_revision"] = json!(9_007_199_254_740_992_u64);
    let mut bound = draft();
    bound["document"]["limits"]["active_executions"] = json!(129);
    let mut trailing = valid;
    trailing.extend_from_slice(b"{}");
    for invalid in [
        duplicate.into_bytes(),
        nested.into_bytes(),
        serde_json::to_vec(&unknown)?,
        serde_json::to_vec(&literal)?,
        serde_json::to_vec(&numeric)?,
        serde_json::to_vec(&bound)?,
        trailing,
        vec![255],
    ] {
        let error = read_json::<ConfigurationPut>(&invalid[..])
            .await
            .err()
            .context("accepted invalid input")?;
        for format in [Format::Json, Format::Human] {
            let output = String::from_utf8(
                Output::new(format).render_to_vec(&envelope_from_anyhow(&error)),
            )?;
            ensure!(!output.contains("SECRET_SENTINEL"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_target_or_token_fails_before_opening_mutation_file() -> Result<()> {
    for (endpoint, token, expected) in [
        (
            "http://SECRET_SENTINEL@localhost",
            Some(TOKEN.into()),
            "user information",
        ),
        (
            "http://localhost",
            Some("SECRET_SENTINEL invalid".into()),
            "Bearer-compatible",
        ),
        (
            "http://localhost",
            None,
            "environment variable is unavailable",
        ),
    ] {
        let action = Action::Configuration {
            action: ConfigurationAction::Apply {
                target: target(endpoint),
                tenant: "Tenant-A".try_into()?,
                input: "/missing-draft.json".into(),
            },
        };
        let error = execute(&action, None, |_| token.clone())
            .await
            .err()
            .context("accepted invalid target or token")?;
        ensure!(
            error.to_string().contains(expected),
            "expected {expected}, received {error}"
        );
        ensure!(!format!("{error:#}").contains("SECRET_SENTINEL"));
    }
    let lookups = std::cell::Cell::new(0);
    for (target, context) in [
        (
            Target {
                endpoint: None,
                token_env: None,
            },
            None,
        ),
        (target("http://localhost"), Some("missing-context")),
        (
            Target {
                endpoint: None,
                token_env: None,
            },
            Some("local"),
        ),
        (
            Target {
                endpoint: Some("http://localhost".into()),
                token_env: Some("${SECRET_SENTINEL}".into()),
            },
            None,
        ),
    ] {
        let action = Action::Capabilities { target };
        ensure!(
            execute(&action, context, |_| {
                lookups.set(lookups.get() + 1);
                None
            })
            .await
            .is_err()
        );
    }
    ensure!(lookups.get() == 0);
    Ok(())
}

#[test]
fn non_applied_operation_reports_are_not_successful() -> Result<()> {
    for status in ["pending", "unknown", "rejected", "applied"] {
        let operation = json!({"operation_id":"prepare-1", "status":status,
            "authority_revision":18, "resource_revision":"configuration-1", "resource_id":"configuration-1",
            "error": if status == "rejected" { json!({"code":"revision_conflict", "message":"stale revision", "dispatch_state":"not_started", "operation_id":"prepare-1", "execution_id":null}) } else { Value::Null }});
        let report = Report::Operation(serde_json::from_value(operation)?);
        ensure!(report.exit_code() == if status == "applied" { 0 } else { 1 });
        let output: Value =
            serde_json::from_slice(&Output::new(Format::Json).render_to_vec(&report))?;
        ensure!(output["status"] == status);
    }
    Ok(())
}
