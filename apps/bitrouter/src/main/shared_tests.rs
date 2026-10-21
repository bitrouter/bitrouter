//! Shared administration rejects ambiguous targets before local discovery.

use super::*;

fn invocation() -> Vec<&'static str> {
    vec![
        "bro",
        "shared",
        "init",
        "--config",
        "missing-config",
        "--plan",
        "missing-plan",
        "--operator-id",
        "operator",
        "--operation-id",
        "root",
    ]
}

fn remote_invocations() -> Vec<Vec<&'static str>> {
    [
        vec!["apply", "--input", "-"],
        vec!["operation", "--operation-id", ".."],
        vec!["revision"],
        vec!["tenant", "--id", "tenant"],
        vec!["resource", "--id", "pool"],
        vec!["principal", "--id", "principal"],
        vec![
            "credential",
            "--provider",
            "provider",
            "--account",
            "account",
        ],
        vec!["execution", "--id", "execution"],
    ]
    .into_iter()
    .map(|action| {
        let mut args = vec!["bro", "shared"];
        args.extend(action);
        args.extend([
            "--endpoint",
            "http://127.0.0.1:4358",
            "--token-env",
            "UNREAD_OPERATOR_TOKEN",
        ]);
        args
    })
    .collect()
}

#[test]
fn remote_operator_commands_require_explicit_targets_and_selectors() -> Result<()> {
    for args in remote_invocations() {
        Cli::try_parse_from(&args)?;
        for flag in [
            "--endpoint",
            "--token-env",
            "--input",
            "--operation-id",
            "--id",
            "--provider",
            "--account",
        ] {
            if let Some(index) = args.iter().position(|arg| *arg == flag) {
                let mut missing = args.clone();
                missing.drain(index..index + 2);
                anyhow::ensure!(Cli::try_parse_from(missing).is_err());
            }
        }
        for unsupported in ["--token", "--config", "--operator-id", "--grant"] {
            let mut ambiguous = args.clone();
            ambiguous.extend([unsupported, "forbidden"]);
            anyhow::ensure!(Cli::try_parse_from(ambiguous).is_err());
        }
    }
    Ok(())
}

#[test]
fn operator_recovery_hints_parse_even_for_leading_hyphen_handles() -> Result<()> {
    for id in ["..", "-job", "--help"] {
        let error = anyhow::Error::new(bitrouter_sdk::error::Error::OperatorUnknown {
            operation_id: id.into(),
            source: Box::new(bitrouter_sdk::error::Error::Response("unavailable")),
        });
        let envelope = bitrouter::output::error::envelope_from_anyhow(&error);
        let argument = format!("--operation-id={id}");
        anyhow::ensure!(
            envelope
                .error
                .hint
                .is_some_and(|hint| hint.contains(&argument))
        );
        let cli = Cli::try_parse_from([
            "bro",
            "shared",
            "operation",
            "--endpoint",
            "http://localhost:4358",
            "--token-env",
            "TOKEN",
            &argument,
        ])?;
        anyhow::ensure!(
            matches!(cli.command, Some(Command::Shared {action:SharedAction::Operator(shared_operator::Action::Operation {operation_id,..})}) if operation_id == id)
        );
    }
    Ok(())
}

#[cfg(feature = "multi-instance")]
#[tokio::test]
async fn invalid_token_env_uses_redacted_runtime_errors_after_real_cli_parsing() -> Result<()> {
    let cli = Cli::try_parse_from([
        "bro",
        "shared",
        "apply",
        "--endpoint",
        "http://localhost:4358",
        "--token-env",
        "PRIVATE_ARGUMENT_SENTINEL+TOKEN",
        "--input",
        "-",
    ])?;
    let Some(Command::Shared {
        action: SharedAction::Operator(action),
    }) = cli.command
    else {
        anyhow::bail!("operator action missing");
    };
    let error = shared_operator::run(action, &Output::from_flags(true, false))
        .await
        .err()
        .context("accepted invalid reference")?;
    anyhow::ensure!(error.to_string() == "invalid operator token environment reference");
    let envelope = bitrouter::output::error::envelope_from_anyhow(&error);
    for format in [
        bitrouter::output::Format::Json,
        bitrouter::output::Format::Human,
    ] {
        let text = String::from_utf8(Output::new(format).render_to_vec(&envelope))?;
        anyhow::ensure!(!text.contains("PRIVATE_ARGUMENT_SENTINEL"));
    }
    Ok(())
}

#[tokio::test]
async fn remote_operator_preflight_rejects_before_input_or_discovery() -> Result<()> {
    for args in remote_invocations() {
        #[cfg(feature = "multi-instance")]
        for context in ["local", "missing-context"] {
            let mut with_context = args.clone();
            with_context.splice(1..1, ["--context", context]);
            let error = run(
                Cli::try_parse_from(with_context)?,
                &Output::from_flags(true, false),
            )
            .await
            .err()
            .context("accepted ambiguous remote operator target")?;
            anyhow::ensure!(error.to_string().contains("does not use named contexts"));
        }
        #[cfg(not(feature = "multi-instance"))]
        {
            let error = run(Cli::try_parse_from(args)?, &Output::from_flags(true, false))
                .await
                .err()
                .context("accepted unavailable remote operator command")?;
            anyhow::ensure!(
                error.to_string() == "shared administration requires the multi-instance feature"
            );
        }
    }
    Ok(())
}

#[test]
fn bootstrap_requires_every_explicit_target_and_recovery_identity() -> Result<()> {
    let args = invocation();
    let cli = Cli::try_parse_from(&args)?;
    anyhow::ensure!(matches!(
        cli.command,
        Some(Command::Shared {
            action: SharedAction::Init {
                global_principals: false,
                ..
            }
        })
    ));
    for index in [3, 5, 7, 9] {
        let mut missing = args.clone();
        missing.drain(index..index + 2);
        anyhow::ensure!(Cli::try_parse_from(missing).is_err());
    }
    Ok(())
}

#[cfg(feature = "multi-instance")]
#[tokio::test]
async fn bootstrap_rejects_named_context_before_reading_any_inputs() -> Result<()> {
    for context in ["local", "a-context-that-does-not-exist"] {
        let mut args = invocation();
        args.splice(1..1, ["--context", context]);
        let error = run(Cli::try_parse_from(args)?, &Output::from_flags(true, false))
            .await
            .err()
            .context("accepted ambiguous bootstrap target")?;
        anyhow::ensure!(error.to_string().contains("does not use named contexts"));
    }
    Ok(())
}

#[cfg(not(feature = "multi-instance"))]
#[tokio::test]
async fn bootstrap_rejects_unavailable_feature_before_reading_any_inputs() -> Result<()> {
    let error = run(
        Cli::try_parse_from(invocation())?,
        &Output::from_flags(true, false),
    )
    .await
    .err()
    .context("accepted unavailable bootstrap")?;
    anyhow::ensure!(
        error.to_string() == "shared administration requires the multi-instance feature"
    );
    Ok(())
}
