//! Owned Rust subprocess fixtures; parent runs these, no PostgreSQL required.

use std::os::unix::process::ExitStatusExt as _;

use super::*;

const CHILD_MODE: &str = "BITROUTER_ADMISSION_CLEANUP_CHILD_MODE";
const CHILD_READY: &str = "BITROUTER_ADMISSION_CLEANUP_CHILD_READY";

fn clean_log() -> &'static str {
    "INFO bitrouter::gateway::shared::runtime: shared gateway shutdown completed forced=false withdrawal=Confirmed quiescent=true durable_resolved=true clean=true\n"
}

fn forced_log() -> &'static str {
    "INFO bitrouter::gateway::shared::runtime: shared gateway shutdown completed forced=true withdrawal=Confirmed quiescent=true durable_resolved=false clean=false\n{\n  \"error\": {\n    \"kind\": \"internal\",\n    \"message\": \"shared gateway stopped with interrupted or unresolved work\"\n  }\n}\n"
}

#[test]
fn observed_unknown_only_allows_the_exact_completed_force_outcome() -> Result<()> {
    let zero = ExitStatus::from_raw(0);
    let one = ExitStatus::from_raw(1 << 8);
    ExpectedExit::CleanServe.check(zero, clean_log())?;
    ExpectedExit::ObservedUnknown.check(zero, clean_log())?;
    ExpectedExit::ObservedUnknown.check(one, forced_log())?;
    ensure!(ExpectedExit::Success.check(one, forced_log()).is_err());
    ensure!(ExpectedExit::CleanServe.check(one, forced_log()).is_err());
    ensure!(ExpectedExit::CleanServe.check(zero, "").is_err());
    ensure!(
        ExpectedExit::ObservedUnknown
            .check(zero, forced_log())
            .is_err()
    );
    for raw in [2 << 8, 7 << 8, 101 << 8, 9, 15] {
        ensure!(
            ExpectedExit::ObservedUnknown
                .check(ExitStatus::from_raw(raw), forced_log())
                .is_err()
        );
    }
    for changed in [
        forced_log().replace("forced=true", "forced=false"),
        forced_log().replace("withdrawal=Confirmed", "withdrawal=Failed"),
        forced_log().replace("withdrawal=Confirmed", "withdrawal=Unknown"),
        forced_log().replace("quiescent=true", "quiescent=false"),
        forced_log().replace("durable_resolved=false", "durable_resolved=true"),
        forced_log().replace("clean=false", "clean=true"),
        forced_log().replace("clean=false", "clean=false clean=false"),
        forced_log().replace(
            "shared gateway stopped with interrupted or unresolved work",
            "shared heartbeat worker exited",
        ),
        forced_log().replace("\"internal\"", "\"upstream\""),
        forced_log().replace("\"kind\":", "\"context\": [\"another error\"], \"kind\":"),
        format!("{}{}", clean_log(), forced_log()),
        format!("{}\n{{\"error\":{{}}}}", forced_log()),
        forced_log()
            .lines()
            .next()
            .context("missing fixture report")?
            .to_owned(),
    ] {
        ensure!(
            ExpectedExit::ObservedUnknown.check(one, &changed).is_err(),
            "an unrelated or incomplete failure passed the Unknown exit exception"
        );
    }
    let color = forced_log()
        .replace("forced=", "\u{1b}[3mforced\u{1b}[0m=")
        .replace("quiescent=", "\u{1b}[3mquiescent\u{1b}[0m=");
    ExpectedExit::ObservedUnknown.check(one, &color)?;
    Ok(())
}

// Spawn this very test binary with an exact filter. No shell/interpreter and no
// unowned PID discovery; the parent helper retains each tokio::process::Child.
#[tokio::test]
async fn cleanup_exit_fixture() -> Result<()> {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return Ok(());
    };
    match mode.as_str() {
        "seven" => std::process::exit(7),
        "twenty-three" => std::process::exit(23),
        "term" => {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            std::fs::write(std::env::var(CHILD_READY)?, "ready")?;
            term.recv().await.context("child TERM stream closed")?;
            println!("FINAL_LOG_AFTER_TERM");
            Ok(())
        }
        _ => anyhow::bail!("unknown cleanup child fixture mode"),
    }
}

fn spawn_child(root: &Path, index: usize, mode: &str) -> Result<Running> {
    let log = root.join(format!("child-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let (_, module) = module_path!()
        .split_once("::")
        .context("missing crate prefix")?;
    let test = format!("{module}::cleanup_exit_fixture");
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(CHILD_MODE, mode)
        .env(CHILD_READY, root.join("child-ready"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true)
        .spawn()?;
    Ok(Running {
        child,
        log,
        address: "127.0.0.1:0".parse()?,
    })
}

#[tokio::test]
async fn nonzero_children_fail_and_all_children_are_joined_with_final_logs() -> Result<()> {
    for primary_failed in [false, true] {
        let root = tempfile::tempdir()?;
        let prefix = root.path().join("retained");
        let mut children = Vec::new();
        let setup = async {
            for mode in ["seven", "twenty-three", "term"] {
                children.push(spawn_child(root.path(), children.len(), mode)?);
            }
            // Exercise the already-exited branch for two failures. The third
            // child stays alive with a registered TERM handler until cleanup.
            ensure!(children[0].exited().await?.code() == Some(7));
            ensure!(children[1].exited().await?.code() == Some(23));
            tokio::time::timeout(Duration::from_secs(3), async {
                while !root.path().join("child-ready").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("cleanup child did not become ready")?;
            ensure!(children[2].child.try_wait()?.is_none());
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let configured = setup.is_ok();
        let primary = setup.and_then(|()| {
            if primary_failed {
                anyhow::bail!("PRIMARY_CAMPAIGN_FAILURE");
            }
            Ok(())
        });
        let result =
            finish_children(&mut children, primary, &ExitExpectations::new(), &prefix).await;
        let error = result.err().context("nonzero child exits were discarded")?;
        ensure!(configured, "child fixture setup failed: {error:#}");
        let diagnostic = format!("{error:#}");
        ensure!(
            diagnostic.contains("joining fixture gateway 0")
                && diagnostic.contains("joining fixture gateway 1")
        );
        ensure!(diagnostic.contains("exit status: 7") && diagnostic.contains("exit status: 23"));
        ensure!(!diagnostic.contains("joining fixture gateway 2"));
        ensure!(diagnostic.contains("PRIMARY_CAMPAIGN_FAILURE") == primary_failed);
        for (index, child) in children.iter_mut().enumerate() {
            ensure!(
                child.child.try_wait()?.is_some(),
                "child {index} was not reaped"
            );
            ensure!(prefix.with_extension(format!("{index}.log")).is_file());
        }
        ensure!(
            std::fs::read_to_string(prefix.with_extension("2.log"))?
                .contains("FINAL_LOG_AFTER_TERM"),
            "failure logs were copied before child cleanup completed"
        );
    }
    Ok(())
}

#[test]
fn copying_failure_keeps_primary_error_and_attempts_every_log() -> Result<()> {
    let root = tempfile::tempdir()?;
    let paths = [
        root.path().join("missing"),
        root.path().join("second"),
        root.path().join("third"),
    ];
    std::fs::write(&paths[1], "second log")?;
    std::fs::write(&paths[2], "third log")?;
    let prefix = root.path().join("retained");
    let error = retain_failed_logs(
        paths.iter().map(PathBuf::as_path),
        Err(anyhow::anyhow!("PRIMARY_CLEANUP_FAILURE")),
        &prefix,
    )
    .err()
    .context("copying failure disappeared")?;
    let diagnostic = format!("{error:#}");
    ensure!(diagnostic.contains("PRIMARY_CLEANUP_FAILURE"));
    ensure!(diagnostic.contains("retaining failed child log"));
    ensure!(std::fs::read_to_string(prefix.with_extension("1.log"))? == "second log");
    ensure!(std::fs::read_to_string(prefix.with_extension("2.log"))? == "third log");
    Ok(())
}
