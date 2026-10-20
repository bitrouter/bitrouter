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
