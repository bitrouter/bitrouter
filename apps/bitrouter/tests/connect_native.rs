//! Cross-client lifecycle checks: offline status, replacement and crash recovery.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const MODEL: &str = "bitrouter/automatos";
const NEXT_MODEL: &str = "team/replacement";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        for name in ["agent/child", "bro", "xdg", "second-agent"] {
            std::fs::create_dir_all(root.join(name))?;
        }
        std::fs::write(
            root.join("agent/settings.json"),
            "{\"defaultProvider\":\"native\",\"defaultModel\":\"original\"}\n",
        )?;
        Ok(Self { _temp: temp, root })
    }

    async fn run(
        &self,
        args: &[&str],
        token: Option<&str>,
        inline: Option<&str>,
        success: bool,
    ) -> Result<Value> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(args)
            .current_dir(&self.root)
            .env("BITROUTER_HOME", self.root.join("bro"))
            .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env_remove("BITROUTER_CONFIG")
            .env_remove("BITROUTER_CONTEXT")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .env_remove("BITROUTER_API_KEY")
            .kill_on_drop(true);
        if let Some(token) = token {
            command.env("BITROUTER_API_KEY", token);
        }
        if let Some(inline) = inline {
            command.env("OPENCODE_CONFIG_CONTENT", inline);
        }
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(20), command.output()).await??;
        assert_eq!(
            output.status.success(),
            success,
            "args={args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if success {
            Ok(serde_json::from_slice(&output.stdout)?)
        } else {
            Ok(Value::Null)
        }
    }

    fn receipt(&self, target: &str) -> Result<PathBuf> {
        for entry in std::fs::read_dir(self.root.join("bro/connections"))? {
            let path = entry?.path();
            let name = path.file_name().context("receipt name")?.to_string_lossy();
            if name.starts_with(&format!("{target}-"))
                && name.ends_with(".json")
                && !name.ends_with(".update.json")
            {
                return Ok(path);
            }
        }
        anyhow::bail!("missing {target} receipt")
    }

    fn snapshot(&self, receipt: &Value) -> Result<BTreeMap<PathBuf, Option<String>>> {
        let mut values = BTreeMap::new();
        for file in receipt["files"].as_array().context("receipt files")? {
            let path = PathBuf::from(file["path"].as_str().context("file path")?);
            let value = match std::fs::read_to_string(&path) {
                Ok(value) => Some(value),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            values.insert(path, value);
        }
        Ok(values)
    }
}

fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

async fn gateway() -> MockServer {
    let server = MockServer::start().await;
    for token in ["fixture-key", "replacement-key"] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"id":MODEL},{"id":NEXT_MODEL}]})),
            )
            .mount(&server)
            .await;
    }
    server
}

#[tokio::test]
async fn status_uses_normalized_instance_and_check_uses_the_saved_credential() -> Result<()> {
    let fixture = Fixture::new()?;
    let initial = fixture
        .run(&["connect", "pi", "--status"], None, None, true)
        .await?;
    assert_eq!(initial["state"], "not_configured");
    let server = gateway().await;
    fixture
        .run(
            &["connect", "pi", "--base-url", &server.uri(), "--yes"],
            Some("fixture-key"),
            None,
            true,
        )
        .await?;
    let alias = fixture.root.join("agent/child/../.");
    let report = fixture
        .run(
            &[
                "connect",
                "pi",
                "--instance",
                alias.to_str().context("instance")?,
                "--status",
            ],
            None,
            None,
            true,
        )
        .await?;
    assert_eq!(report["state"], "already_configured");
    assert_eq!(report["configuration_verified"], true);
    assert_eq!(report["gateway_catalog_verified"], false);
    let checked = fixture
        .run(
            &["connect", "pi", "--check"],
            Some("incorrect-shell-key"),
            None,
            true,
        )
        .await?;
    assert_eq!(checked["state"], "catalog_verified");
    assert_eq!(checked["gateway_catalog_verified"], true);
    assert_eq!(checked["inference_verified"], false);
    let second = fixture
        .run(
            &[
                "connect",
                "pi",
                "--instance",
                fixture
                    .root
                    .join("second-agent")
                    .to_str()
                    .context("second instance")?,
                "--status",
            ],
            None,
            None,
            true,
        )
        .await?;
    assert_eq!(second["state"], "not_configured");
    assert_eq!(
        server.received_requests().await.context("requests")?.len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn update_preserves_connection_on_catalog_and_builder_failure() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture
        .run(
            &["connect", "pi", "--base-url", &server.uri(), "--yes"],
            Some("fixture-key"),
            None,
            true,
        )
        .await?;
    let receipt_path = fixture.receipt("pi")?;
    let original_receipt = std::fs::read(&receipt_path)?;
    let original_files = fixture.snapshot(&read_json(&receipt_path)?)?;
    fixture
        .run(
            &[
                "connect",
                "pi",
                "--update",
                "--model",
                "absent/model",
                "--yes",
            ],
            Some("replacement-key"),
            None,
            false,
        )
        .await?;
    assert_eq!(std::fs::read(&receipt_path)?, original_receipt);
    assert_eq!(
        fixture.snapshot(&read_json(&receipt_path)?)?,
        original_files
    );

    let config = fixture.root.join("opencode.json");
    let instance = config.to_str().context("OpenCode instance")?;
    fixture
        .run(
            &[
                "connect",
                "opencode",
                "--instance",
                instance,
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            Some("fixture-key"),
            None,
            true,
        )
        .await?;
    let receipt_path = fixture.receipt("opencode")?;
    let original_receipt = std::fs::read(&receipt_path)?;
    let original_config = std::fs::read(&config)?;
    let collision = "{\"provider\":{\"bitrouter\":{\"models\":{}}}}";
    fixture
        .run(
            &[
                "connect",
                "opencode",
                "--instance",
                instance,
                "--update",
                "--model",
                NEXT_MODEL,
                "--yes",
            ],
            Some("replacement-key"),
            Some(collision),
            false,
        )
        .await?;
    assert_eq!(
        read_json(&receipt_path)?,
        serde_json::from_slice::<Value>(&original_receipt)?
    );
    assert_eq!(std::fs::read(&config)?, original_config);
    assert!(!receipt_path.with_extension("update.json").exists());
    Ok(())
}

#[tokio::test]
async fn automatic_opencode_instance_refuses_a_shadowing_project_provider() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    let project = fixture.root.join("opencode.json");
    let original = json!({"provider":{"bitrouter":{"options":{"baseURL":"https://project.invalid/v1"},"models":{}}}}).to_string();
    std::fs::write(&project, &original)?;
    fixture
        .run(
            &["connect", "opencode", "--base-url", &server.uri(), "--yes"],
            Some("fixture-key"),
            None,
            false,
        )
        .await?;
    assert_eq!(std::fs::read_to_string(project)?, original);
    assert!(!fixture.root.join("xdg/opencode/opencode.json").exists());
    assert!(
        server
            .received_requests()
            .await
            .context("requests")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn replacement_recovery_resumes_after_part_of_the_old_connection_was_restored() -> Result<()>
{
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture
        .run(
            &["connect", "pi", "--base-url", &server.uri(), "--yes"],
            Some("fixture-key"),
            None,
            true,
        )
        .await?;
    let receipt_path = fixture.receipt("pi")?;
    let previous = read_json(&receipt_path)?;
    let old_files = fixture.snapshot(&previous)?;
    fixture
        .run(
            &[
                "connect",
                "pi",
                "--update",
                "--model",
                NEXT_MODEL,
                "--display-name",
                "Replacement",
                "--yes",
            ],
            Some("replacement-key"),
            None,
            true,
        )
        .await?;
    let next = read_json(&receipt_path)?;
    let mut rollback = previous.clone();
    let mut rollback_files = vec![];
    for file in next["files"].as_array().context("replacement files")? {
        let path = PathBuf::from(file["path"].as_str().context("path")?);
        let before = old_files.get(&path).context("old snapshot")?;
        let restored = file["before"].as_str().map(str::to_owned);
        // Model the durable phase after the new connection was unwound.
        match &restored {
            Some(text) => std::fs::write(&path, text)?,
            None if path.exists() => std::fs::remove_file(&path)?,
            None => {}
        }
        rollback_files.push(json!({"path":path,"before":before,"after":restored.unwrap_or_default(),"restore":{"kind":"exact"}}));
    }
    rollback["files"] = Value::Array(rollback_files);
    // The first rollback write reached disk before a simulated process exit.
    let (first_path, first_value) = old_files.iter().next().context("first snapshot")?;
    if let Some(text) = first_value {
        std::fs::write(first_path, text)?;
    }
    let journal = receipt_path.with_extension("update.json");
    std::fs::write(
        &journal,
        serde_json::to_vec_pretty(
            &json!({"previous":previous,"rollback":rollback,"active_restored":true}),
        )?,
    )?;
    fixture
        .run(&["connect", "pi", "--status"], None, None, false)
        .await?;
    let recovered = fixture
        .run(&["connect", "pi", "--yes"], None, None, true)
        .await?;
    assert_eq!(recovered["model"], MODEL);
    assert_eq!(recovered["configuration_verified"], true);
    assert_eq!(fixture.snapshot(&read_json(&receipt_path)?)?, old_files);
    assert!(!journal.exists());
    fixture
        .run(&["connect", "pi", "--remove", "--yes"], None, None, true)
        .await?;
    assert_eq!(
        read_json(&fixture.root.join("agent/settings.json"))?,
        json!({"defaultProvider":"native","defaultModel":"original"})
    );
    Ok(())
}
