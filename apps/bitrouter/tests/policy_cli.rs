//! Current-source CLI lifecycle, explicit inspection, CAS and rejection boundaries.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::policy::Document;

fn invoke(home: &Path, args: &[&str]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .args(args)
        .output()
        .with_context(|| format!("running bro {}", args.join(" ")))
}

fn json(home: &Path, args: &[&str]) -> Result<serde_json::Value> {
    let output = invoke(home, args)?;
    ensure!(
        output.status.success(),
        "bro {args:?}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("decoding CLI report")
}

fn starter(home: &Path) -> Result<()> {
    std::fs::write(
        home.join("bitrouter.yaml"),
        r#"inherit_defaults: false
server:
  control_socket: ./bitrouter.sock
providers:
  fixture:
    api_base: https://fixture.invalid/v1
    api_key: fixture-key
    auto_discover: false
    models:
      - id: strong
      - id: economy
"#,
    )
    .context("writing isolated provider configuration")
}

fn initialize(home: &Path) -> Result<serde_json::Value> {
    json(
        home,
        &[
            "policy",
            "init",
            "coding",
            "--strong",
            "fixture:strong",
            "--economy",
            "fixture:economy",
            "--config",
            "bitrouter.yaml",
        ],
    )
}

fn exported(home: &Path) -> Result<(Document, String)> {
    let report = json(
        home,
        &[
            "policy",
            "show",
            "coding",
            "--disk",
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(report["view"] == "disk", "wrong source view: {report}");
    let digest = report["source_digest"]
        .as_str()
        .context("missing source digest")?
        .to_string();
    let document = serde_json::from_value(report["source"].clone())
        .context("show did not export typed source")?;
    ensure!(
        report.get("mode").is_none(),
        "retired mode leaked into report"
    );
    Ok((document, digest))
}

fn candidate(home: &Path, document: &Document) -> Result<Document> {
    let mut next = document.clone();
    next.policies
        .get_mut("coding")
        .context("missing initialized policy")?
        .default_tier = "economy".into();
    std::fs::write(home.join("candidate.yaml"), serde_saphyr::to_string(&next)?)?;
    Ok(next)
}

#[test]
fn source_cli_init_check_export_diff_publish_and_rollback() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    let initialized = initialize(home)?;
    ensure!(initialized["config_activation"] == "saved_only");
    ensure!(home.join("policy.yaml").is_file() && !home.join("policy-lock.yaml").exists());
    let config_after_init = std::fs::read(home.join("bitrouter.yaml"))?;
    json(home, &["policy", "check", "--config", "bitrouter.yaml"])?;
    let (original, parent_digest) = exported(home)?;
    let disk: Document =
        serde_saphyr::from_str(&std::fs::read_to_string(home.join("policy.yaml"))?)?;
    ensure!(
        disk == original,
        "show exported a partial or altered source"
    );
    ensure!(
        original
            .policies
            .get("coding")
            .context("missing initialized policy")?
            .default_tier
            == "strong"
    );
    let next = candidate(home, &original)?;
    let diff = json(home, &["policy", "diff", "policy.yaml", "candidate.yaml"])?;
    ensure!(
        diff["changes"]
            .as_array()
            .is_some_and(|changes| !changes.is_empty())
    );
    ensure!(serde_json::from_value::<Document>(diff["policy"].clone())? == next);
    let published = json(
        home,
        &[
            "policy",
            "publish",
            "candidate.yaml",
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(published["applied"] == true);
    let (active_disk, child_digest) = exported(home)?;
    ensure!(active_disk == next && child_digest != parent_digest);
    let bytes = std::fs::read(home.join("policy.yaml"))?;
    let stale = invoke(
        home,
        &[
            "policy",
            "publish",
            "candidate.yaml",
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(!stale.status.success(), "stale CAS publication succeeded");
    ensure!(
        std::fs::read(home.join("policy.yaml"))? == bytes,
        "stale writer changed source"
    );
    let stale_rollback = invoke(
        home,
        &[
            "policy",
            "rollback",
            &parent_digest,
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(
        !stale_rollback.status.success(),
        "stale CAS rollback succeeded"
    );
    ensure!(std::fs::read(home.join("policy.yaml"))? == bytes);
    json(
        home,
        &[
            "policy",
            "rollback",
            &parent_digest,
            "--expected-digest",
            &child_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    let (restored, restored_digest) = exported(home)?;
    ensure!(restored == original && restored_digest == parent_digest);
    ensure!(
        std::fs::read(home.join("bitrouter.yaml"))? == config_after_init,
        "publication rewrote unrelated configuration"
    );
    Ok(())
}

#[test]
fn active_reads_never_fall_back_to_disk_and_explicit_reload_must_reach_a_daemon() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    initialize(home)?;
    exported(home)?;
    for args in [
        vec!["policy", "show", "coding", "--config", "bitrouter.yaml"],
        vec!["policy", "status", "--config", "bitrouter.yaml"],
        vec!["policy", "reload", "--config", "bitrouter.yaml"],
    ] {
        let output = invoke(home, &args)?;
        ensure!(
            !output.status.success(),
            "unavailable active instance silently succeeded: {args:?}"
        );
    }
    let missing_name = invoke(
        home,
        &[
            "policy",
            "show",
            "absent",
            "--disk",
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(
        !missing_name.status.success(),
        "unknown literal policy name accepted"
    );
    Ok(())
}

#[test]
fn a_named_show_exports_the_complete_source_document() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    initialize(home)?;
    let (mut source, _) = exported(home)?;
    let another = source
        .policies
        .get("coding")
        .context("missing initialized policy")?
        .clone();
    source.policies.insert("another".into(), another);
    std::fs::write(home.join("policy.yaml"), serde_saphyr::to_string(&source)?)?;
    let (shown, digest) = exported(home)?;
    ensure!(
        shown == source && shown.policies.len() == 2,
        "named show exported a subset"
    );
    ensure!(digest == bitrouter::policy_lock::semantic_digest(&shown)?);
    Ok(())
}

#[test]
fn publication_with_an_explicit_unreachable_socket_restores_previous_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    initialize(home)?;
    let (original, parent_digest) = exported(home)?;
    candidate(home, &original)?;
    let rejected = invoke(
        home,
        &[
            "policy",
            "publish",
            "candidate.yaml",
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
            "--socket",
            "missing.sock",
        ],
    )?;
    ensure!(
        !rejected.status.success(),
        "explicit unreachable reload target was ignored"
    );
    let envelope: serde_json::Value = serde_json::from_slice(&rejected.stdout)
        .context("decoding structured publication failure")?;
    ensure!(
        envelope["error"]["context"]
            .as_array()
            .is_some_and(|layers| layers.iter().any(|layer| layer
                .as_str()
                .is_some_and(|detail| detail.contains("restored previous policy source")))),
        "missing recovery explanation: {envelope}"
    );
    let (restored, digest) = exported(home)?;
    ensure!(restored == original && digest == parent_digest);
    Ok(())
}

#[test]
fn rejected_rollback_restores_the_source_that_preceded_the_rollback() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    initialize(home)?;
    let (original, parent_digest) = exported(home)?;
    let next = candidate(home, &original)?;
    json(
        home,
        &[
            "policy",
            "publish",
            "candidate.yaml",
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    let (_, child_digest) = exported(home)?;
    let rejected = invoke(
        home,
        &[
            "policy",
            "rollback",
            &parent_digest,
            "--expected-digest",
            &child_digest,
            "--config",
            "bitrouter.yaml",
            "--socket",
            "missing.sock",
        ],
    )?;
    ensure!(
        !rejected.status.success(),
        "rollback ignored its explicit reload target"
    );
    let (restored, digest) = exported(home)?;
    ensure!(
        restored == next && digest == child_digest,
        "rejected rollback left the rejected source on disk"
    );
    Ok(())
}

#[test]
fn invalid_candidates_do_not_change_active_source_or_configuration() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    initialize(home)?;
    let (_, digest) = exported(home)?;
    let active = std::fs::read(home.join("policy.yaml"))?;
    let config = std::fs::read(home.join("bitrouter.yaml"))?;
    let valid = String::from_utf8(active.clone())?;
    let mut invalid_tier: Document = serde_saphyr::from_str(&valid)?;
    invalid_tier
        .policies
        .get_mut("coding")
        .context("missing initialized policy")?
        .default_tier = "absent".into();
    for invalid in [
        "lockfileVersion: 4\npolicies: {}\n".to_string(),
        serde_saphyr::to_string(&invalid_tier)?,
        format!("{valid}\nartifact: {{}}\n"),
    ] {
        std::fs::write(home.join("candidate.yaml"), invalid)?;
        let rejected = invoke(
            home,
            &[
                "policy",
                "publish",
                "candidate.yaml",
                "--expected-digest",
                &digest,
                "--config",
                "bitrouter.yaml",
            ],
        )?;
        ensure!(
            !rejected.status.success(),
            "invalid candidate publication succeeded"
        );
        ensure!(std::fs::read(home.join("policy.yaml"))? == active);
        ensure!(std::fs::read(home.join("bitrouter.yaml"))? == config);
    }
    Ok(())
}

#[test]
fn unsupported_sources_and_configuration_are_rejected_without_rewriting() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let config = home.join("bitrouter.yaml");
    let policy = home.join("policy.yaml");
    std::fs::write(
        &config,
        "inherit_defaults: true\npolicy:\n  path: policy.yaml\n",
    )?;
    let missing = invoke(home, &["policy", "check", "--config", "bitrouter.yaml"])?;
    ensure!(
        !missing.status.success(),
        "explicit missing source fell back to bundled source"
    );
    let source = "spec_version: policy/1.0.0-beta.1\npolicies:\n  coding:\n    tiers:\n      strong: {model: 'fixture:strong'}\n    default_tier: strong\n";
    let mut rejected = (1..=4)
        .map(|version| format!("lockfileVersion: {version}\npolicies: {{}}\n"))
        .collect::<Vec<_>>();
    rejected.extend([
        source.replace("spec_version: policy/1.0.0-beta.1\n", ""),
        source.replace("policy/1.0.0-beta.1", "policy/1.0.0-beta.99"),
        source.replace(
            "model: 'fixture:strong'",
            "model: 'fixture:strong', context: preserve",
        ),
        source.replace(
            "    default_tier: strong",
            "    default_tier: strong\n    default_tier: strong",
        ),
        source.replace(
            "    default_tier: strong",
            "    default_tier: strong\n    progress_guard: {}",
        ),
    ]);
    for raw in rejected {
        std::fs::write(&policy, &raw)?;
        let output = invoke(home, &["policy", "check", "--config", "bitrouter.yaml"])?;
        ensure!(
            !output.status.success(),
            "unsupported source accepted: {raw}"
        );
        ensure!(
            std::fs::read_to_string(&policy)? == raw,
            "rejection rewrote source"
        );
    }
    std::fs::write(&policy, source)?;
    for raw in [
        "presets: {}\n",
        "variants: {}\n",
        "policy:\n  mode: frozen\n",
        "policy:\n  writeback: evolve\n",
    ] {
        std::fs::write(&config, raw)?;
        let output = invoke(home, &["config", "validate", "--config", "bitrouter.yaml"])?;
        ensure!(!output.status.success(), "retired config accepted: {raw}");
        ensure!(std::fs::read_to_string(&config)? == raw);
        ensure!(std::fs::read_to_string(&policy)? == source);
    }
    Ok(())
}

#[test]
fn acl_creation_has_an_independent_namespace() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let legacy = invoke(home, &["policy", "create", "readers"])?;
    ensure!(!legacy.status.success() && !home.join("policies").exists());
    let report = json(home, &["access-policy", "create", "readers"])?;
    ensure!(report["created"] == true && home.join("policies/readers.yaml").is_file());
    ensure!(
        !home.join("policy.yaml").exists(),
        "ACL creation wrote a model policy source"
    );
    ensure!(
        !invoke(home, &["access-policy", "create", "readers"])?
            .status
            .success()
    );
    Ok(())
}

#[test]
fn shipped_coding_template_initializes_and_exports_current_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    let fixture = std::fs::read_to_string(home.join("bitrouter.yaml"))?;
    let fixture = fixture
        .strip_prefix("inherit_defaults: false\n")
        .context("missing fixture configuration prefix")?;
    std::fs::write(
        home.join("bitrouter.yaml"),
        format!(
            "{}\n{fixture}",
            include_str!("../../../templates/coding-router/bitrouter.yaml"),
        ),
    )?;
    initialize(home)?;
    json(home, &["policy", "check", "--config", "bitrouter.yaml"])?;
    let (source, _) = exported(home)?;
    ensure!(source.policies.contains_key("coding"));
    ensure!(home.join("policy.yaml").is_file());
    Ok(())
}

#[test]
fn shipped_auto_template_uses_the_same_resubmittable_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    std::fs::write(
        home.join("bitrouter.yaml"),
        include_str!("../../../templates/auto-router/bitrouter.yaml"),
    )?;
    std::fs::write(
        home.join("policy.yaml"),
        include_str!("../../../templates/auto-router/policy.yaml"),
    )?;
    json(home, &["policy", "check", "--config", "bitrouter.yaml"])?;
    let report = json(
        home,
        &[
            "policy",
            "show",
            "auto",
            "--disk",
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    let source: Document = serde_json::from_value(report["source"].clone())?;
    let bundled: Document = serde_saphyr::from_str(include_str!("../policy/default.yaml"))?;
    ensure!(
        source == bundled,
        "template and bundled defaults have different source semantics"
    );
    let auto = source.policies.get("auto").context("missing auto policy")?;
    ensure!(auto.default_tier == "strong");
    ensure!(auto.tiers.len() == 3 && auto.routes.is_empty());
    Ok(())
}

/// One real process carries source identity through publication, reload and rollback.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_cli_publication_reload_and_rollback_keep_disk_and_runtime_equal() -> Result<()> {
    use std::{process::Stdio, time::Duration};

    struct Foreground(std::process::Child);
    impl Drop for Foreground {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    let directory = tempfile::tempdir()?;
    let home = directory.path();
    starter(home)?;
    let configuration = home.join("bitrouter.yaml");
    let fixture = std::fs::read_to_string(&configuration)?;
    let fixture = fixture
        .strip_prefix("inherit_defaults: false\n")
        .context("missing fixture configuration prefix")?;
    let mut raw = format!(
        "{}\n{fixture}",
        include_str!("../../../templates/coding-router/bitrouter.yaml")
    );
    raw = raw.replace(
        "server:\n  control_socket:",
        "registry: {enabled: false}\ndatabase: {url: 'sqlite://gateway.db?mode=rwc'}\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\n  control_socket:",
    );
    std::fs::write(&configuration, raw)?;
    initialize(home)?;
    let (original, parent_digest) = exported(home)?;
    let original_bytes = std::fs::read(home.join("policy.yaml"))?;
    let next = candidate(home, &original)?;
    let socket = home.join("bitrouter.sock");
    let log = home.join("daemon.stderr");
    let mut daemon = Foreground(
        Command::new(env!("CARGO_BIN_EXE_bitrouter"))
            .env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("BITROUTER_HOME", home)
            .current_dir(home)
            .args(["serve", "--config", "bitrouter.yaml"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log)?)
            .spawn()?,
    );
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            ensure!(
                daemon.0.try_wait()?.is_none(),
                "foreground exited: {}",
                std::fs::read_to_string(&log)?
            );
            if bitrouter::daemon::probe_status(&socket).await?.is_some() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let active = |expected: &Document, digest: &str| -> Result<()> {
        let report = json(
            home,
            &["policy", "show", "coding", "--config", "bitrouter.yaml"],
        )?;
        ensure!(report["view"] == "active");
        ensure!(report["source_digest"] == digest);
        ensure!(serde_json::from_value::<Document>(report["source"].clone())? == *expected);
        ensure!(exported(home)?.0 == *expected);
        Ok(())
    };
    active(&original, &parent_digest)?;
    json(home, &["policy", "check", "--config", "bitrouter.yaml"])?;
    let diff = json(home, &["policy", "diff", "policy.yaml", "candidate.yaml"])?;
    ensure!(serde_json::from_value::<Document>(diff["policy"].clone())? == next);
    let published = json(
        home,
        &[
            "policy",
            "publish",
            "candidate.yaml",
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(published["applied"] == true);
    let (_, child_digest) = exported(home)?;
    active(&next, &child_digest)?;
    json(home, &["policy", "reload", "--config", "bitrouter.yaml"])?;
    json(home, &["reload", "--config", "bitrouter.yaml"])?;
    active(&next, &child_digest)?;
    let stale = invoke(
        home,
        &[
            "policy",
            "publish",
            "candidate.yaml",
            "--expected-digest",
            &parent_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    ensure!(!stale.status.success(), "stale live publication succeeded");
    active(&next, &child_digest)?;
    json(
        home,
        &[
            "policy",
            "rollback",
            &parent_digest,
            "--expected-digest",
            &child_digest,
            "--config",
            "bitrouter.yaml",
        ],
    )?;
    active(&original, &parent_digest)?;
    ensure!(std::fs::read(home.join("policy.yaml"))? == original_bytes);
    bitrouter::daemon::send_command(&socket, &bitrouter::daemon::DaemonCommand::Stop).await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = daemon.0.try_wait()? {
                ensure!(status.success(), "foreground shutdown failed: {status}");
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    Ok(())
}
