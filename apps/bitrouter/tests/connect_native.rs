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
            .env("HOME", &self.root)
            .env("HERMES_HOME", self.root.join("hermes"))
            .env("DSH_HOME", self.root.join("harness"))
            .env("OPENCLAW_STATE_DIR", self.root.join("claw"))
            .env_remove("OPENCLAW_CONFIG_PATH")
            .env_remove("OPENCLAW_HOME")
            .env_remove("OPENCLAW_CONFIG_READONLY")
            .env_remove("OPENCLAW_NIX_MODE")
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

impl Fixture {
    fn initialize_clients(&self) -> Result<()> {
        for directory in [
            "hermes",
            "harness/profiles/headless",
            "claw/agents/main/agent",
            "claw/cron",
        ] {
            std::fs::create_dir_all(self.root.join(directory))?;
        }
        std::fs::write(
            self.root.join("hermes/config.yaml"),
            "model: {provider: native, default: original}\nauxiliary: {compression: {provider: native, model: keep}}\n",
        )?;
        std::fs::write(self.root.join("harness/profiles/headless/package.json"), json!({"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-headless"]}}}).to_string())?;
        std::fs::write(
            self.root.join("harness/profiles/headless/cordis.patch.yml"),
            "- id: agent-default-model\n  config: {provider: native, model: original}\n",
        )?;
        std::fs::write(self.root.join("claw/openclaw.json"), json!({"agents":{"defaults":{"model":{"primary":"native/original"},"modelPolicy":{"allow":[]}}}}).to_string())?;
        std::fs::write(
            self.root.join("claw/cron/jobs.json"),
            "{\"jobs\":[{\"id\":\"existing\",\"payload\":{\"model\":\"native/original\"}}]}\n",
        )?;
        Ok(())
    }

    fn config_path(&self, target: &str) -> PathBuf {
        self.root.join(match target {
            "pi" => "agent/models.json",
            "opencode" => "xdg/opencode/opencode.json",
            "hermes" => "hermes/config.yaml",
            "deepseek-harness" => "harness/profiles/headless/cordis.patch.yml",
            _ => "claw/openclaw.json",
        })
    }

    fn config(&self, target: &str) -> Result<Value> {
        let text = std::fs::read_to_string(self.config_path(target))?;
        if matches!(target, "hermes" | "deepseek-harness") {
            Ok(serde_saphyr::from_str(&text)?)
        } else {
            Ok(json5::from_str(&text)?)
        }
    }

    fn model_ids(&self, target: &str) -> Result<Vec<String>> {
        let config = self.config(target)?;
        let models = match target {
            "pi" => config.pointer("/providers/bitrouter/models"),
            "opencode" => config.pointer("/provider/bitrouter/models"),
            "hermes" => config.pointer("/providers/bitrouter/models"),
            "deepseek-harness" => config
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == "llm-pi-ai"))
                .and_then(|row| row.pointer("/config/providers/bitrouter/models")),
            _ => config.pointer("/models/providers/bitrouter/models"),
        }
        .context("missing native model catalog")?;
        if target == "hermes" {
            Ok(models
                .as_object()
                .context("Hermes models")?
                .keys()
                .cloned()
                .collect())
        } else {
            let entries: Vec<_> = if let Some(object) = models.as_object() {
                object.values().collect()
            } else {
                models.as_array().context("native models")?.iter().collect()
            };
            entries
                .into_iter()
                .map(|entry| {
                    entry["id"]
                        .as_str()
                        .map(str::to_owned)
                        .context("native model ID")
                })
                .collect()
        }
    }

    fn select_secondary(&self, target: &str) -> Result<()> {
        if target == "pi" {
            std::fs::write(
                self.root.join("agent/settings.json"),
                json!({"defaultProvider":"bitrouter","defaultModel":NEXT_MODEL,"theme":"keep"})
                    .to_string(),
            )?;
            return Ok(());
        }
        let mut value = self.config(target)?;
        match target {
            "hermes" => {
                value["model"] = json!({"provider":"custom:bitrouter","default":NEXT_MODEL})
            }
            "opencode" => value["model"] = json!(format!("bitrouter/{NEXT_MODEL}")),
            "deepseek-harness" => {
                let rows = value.as_array_mut().context("Harness rows")?;
                let row = rows
                    .iter_mut()
                    .find(|row| row["id"] == "agent-default-model")
                    .context("Harness default")?;
                row["config"] = json!({"provider":"bitrouter","model":NEXT_MODEL});
            }
            _ => {
                value["agents"]["defaults"]["model"]["primary"] =
                    json!(format!("bitrouter/{NEXT_MODEL}"))
            }
        }
        let text = if matches!(target, "hermes" | "deepseek-harness") {
            serde_saphyr::to_string(&value)?
        } else {
            serde_json::to_string_pretty(&value)?
        };
        std::fs::write(self.config_path(target), text)?;
        Ok(())
    }
}

#[tokio::test]
async fn multiple_selectors_update_check_and_restore_across_native_clients() -> Result<()> {
    for target in ["pi", "opencode", "deepseek-harness", "hermes", "openclaw"] {
        let fixture = Fixture::new()?;
        fixture.initialize_clients()?;
        let jobs = std::fs::read(fixture.root.join("claw/cron/jobs.json"))?;
        let server = gateway().await;
        let report = fixture
            .run(
                &[
                    "connect",
                    target,
                    "--base-url",
                    &server.uri(),
                    "--model",
                    MODEL,
                    "--model",
                    NEXT_MODEL,
                    "--yes",
                ],
                Some("fixture-key"),
                None,
                true,
            )
            .await?;
        assert_eq!(report["models"], json!([MODEL, NEXT_MODEL]), "{target}");
        let mut actual = fixture.model_ids(target)?;
        actual.sort();
        let mut expected = vec![MODEL.to_owned(), NEXT_MODEL.to_owned()];
        expected.sort();
        assert_eq!(actual, expected, "{target}");
        fixture.select_secondary(target)?;
        fixture
            .run(&["connect", target, "--check"], None, None, true)
            .await?;
        let updated = fixture
            .run(&["connect", target, "--update", "--yes"], None, None, true)
            .await?;
        assert_eq!(updated["models"], json!([MODEL, NEXT_MODEL]), "{target}");
        fixture.select_secondary(target)?;
        fixture
            .run(&["connect", target, "--remove", "--yes"], None, None, true)
            .await?;
        let current = std::fs::read_to_string(if target == "pi" {
            fixture.root.join("agent/settings.json")
        } else {
            fixture.config_path(target)
        })?;
        assert!(
            !current.contains(NEXT_MODEL),
            "stale secondary selection after {target} removal: {current}"
        );
        assert_eq!(
            std::fs::read(fixture.root.join("claw/cron/jobs.json"))?,
            jobs
        );
    }
    Ok(())
}

#[tokio::test]
async fn selector_sets_shrink_and_reject_missing_routes_without_mutation() -> Result<()> {
    for target in ["pi", "opencode", "deepseek-harness", "hermes", "openclaw"] {
        let fixture = Fixture::new()?;
        fixture.initialize_clients()?;
        let server = gateway().await;
        fixture
            .run(
                &[
                    "connect",
                    target,
                    "--base-url",
                    &server.uri(),
                    "--model",
                    MODEL,
                    "--model",
                    NEXT_MODEL,
                    "--yes",
                ],
                Some("fixture-key"),
                None,
                true,
            )
            .await?;
        let receipt = read_json(&fixture.receipt(target)?)?;
        let before = fixture.snapshot(&receipt)?;
        fixture
            .run(
                &[
                    "connect",
                    target,
                    "--update",
                    "--model",
                    MODEL,
                    "--model",
                    "unadvertised/route",
                    "--yes",
                ],
                None,
                None,
                false,
            )
            .await?;
        assert_eq!(fixture.snapshot(&receipt)?, before, "{target}");
        assert_eq!(read_json(&fixture.receipt(target)?)?, receipt, "{target}");
        let updated = fixture
            .run(
                &["connect", target, "--update", "--model", MODEL, "--yes"],
                None,
                None,
                true,
            )
            .await?;
        assert_eq!(updated["models"], json!([MODEL]), "{target}");
        assert_eq!(
            fixture.model_ids(target)?,
            vec![MODEL.to_owned()],
            "{target}"
        );
        fixture
            .run(&["connect", target, "--remove", "--yes"], None, None, true)
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn legacy_single_model_receipts_upgrade_without_losing_restore_state() -> Result<()> {
    for target in ["pi", "opencode", "deepseek-harness", "hermes", "openclaw"] {
        let fixture = Fixture::new()?;
        fixture.initialize_clients()?;
        let server = gateway().await;
        fixture
            .run(
                &["connect", target, "--base-url", &server.uri(), "--yes"],
                Some("fixture-key"),
                None,
                true,
            )
            .await?;
        let path = fixture.receipt(target)?;
        let mut legacy = read_json(&path)?;
        legacy["version"] = json!(6);
        legacy["native"]
            .as_object_mut()
            .context("native receipt")?
            .remove("models");
        for file in legacy["files"].as_array_mut().context("receipt files")? {
            if let Some(restore) = file.get_mut("restore").and_then(Value::as_object_mut) {
                restore.remove("models");
            }
        }
        std::fs::write(&path, serde_json::to_vec(&legacy)?)?;
        let status = fixture
            .run(&["connect", target, "--status"], None, None, true)
            .await?;
        assert_eq!(status["models"], json!([MODEL]), "{target}");
        fixture
            .run(
                &[
                    "connect", target, "--update", "--model", MODEL, "--model", NEXT_MODEL, "--yes",
                ],
                None,
                None,
                true,
            )
            .await?;
        let current = read_json(&path)?;
        assert_eq!(current["version"], 7);
        assert_eq!(current["native"]["models"], json!([MODEL, NEXT_MODEL]));
        fixture.select_secondary(target)?;
        fixture
            .run(&["connect", target, "--remove", "--yes"], None, None, true)
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn an_openclaw_primary_alias_does_not_authorize_additional_models() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize_clients()?;
    let path = fixture.config_path("openclaw");
    let mut config = fixture.config("openclaw")?;
    config["agents"]["defaults"]["modelPolicy"]["allow"] = json!(["automatos"]);
    let original = serde_json::to_vec(&config)?;
    std::fs::write(&path, &original)?;
    let server = gateway().await;
    fixture
        .run(
            &[
                "connect",
                "openclaw",
                "--base-url",
                &server.uri(),
                "--model",
                MODEL,
                "--model",
                NEXT_MODEL,
                "--yes",
            ],
            Some("fixture-key"),
            None,
            false,
        )
        .await?;
    assert_eq!(std::fs::read(&path)?, original);
    assert!(
        !fixture
            .root
            .join("claw/bitrouter-connect-credential.json")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn check_validates_the_entire_registered_set_and_duplicates_are_rejected() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture
        .run(
            &[
                "connect",
                "pi",
                "--base-url",
                &server.uri(),
                "--model",
                MODEL,
                "--model",
                MODEL,
                "--yes",
            ],
            Some("fixture-key"),
            None,
            false,
        )
        .await?;
    assert!(!fixture.root.join("agent/models.json").exists());
    fixture
        .run(
            &[
                "connect",
                "pi",
                "--base-url",
                &server.uri(),
                "--model",
                MODEL,
                "--model",
                NEXT_MODEL,
                "--yes",
            ],
            Some("fixture-key"),
            None,
            true,
        )
        .await?;
    let receipt = read_json(&fixture.receipt("pi")?)?;
    let original = fixture.snapshot(&receipt)?;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":MODEL}]})))
        .mount(&server)
        .await;
    fixture
        .run(&["connect", "pi", "--check"], None, None, false)
        .await?;
    assert_eq!(fixture.snapshot(&receipt)?, original);
    Ok(())
}

#[tokio::test]
async fn unsupported_recovery_receipts_are_rejected_before_any_write() -> Result<()> {
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
    let path = fixture.receipt("pi")?;
    let original = read_json(&path)?;
    let before = fixture.snapshot(&original)?;
    let mut previous = original.clone();
    previous["version"] = json!(8);
    let journal = json!({"previous":previous,"rollback":{"version":7,"endpoint":server.uri(),"model":MODEL,"files":[]},"active_restored":true});
    let pending = path.with_extension("update.json");
    std::fs::write(&pending, serde_json::to_vec(&journal)?)?;
    fixture
        .run(&["connect", "pi", "--yes"], None, None, false)
        .await?;
    assert_eq!(read_json(&path)?, original);
    assert_eq!(fixture.snapshot(&original)?, before);
    assert_eq!(read_json(&pending)?, journal);
    Ok(())
}

#[tokio::test]
async fn additional_native_key_collisions_are_rejected_before_configuration() -> Result<()> {
    for target in ["opencode", "openclaw"] {
        let fixture = Fixture::new()?;
        fixture.initialize_clients()?;
        let server = gateway().await;
        fixture
            .run(
                &[
                    "connect",
                    target,
                    "--base-url",
                    &server.uri(),
                    "--model",
                    MODEL,
                    "--model",
                    "automatos",
                    "--yes",
                ],
                Some("fixture-key"),
                None,
                false,
            )
            .await?;
        assert!(
            server
                .received_requests()
                .await
                .context("catalog requests")?
                .is_empty(),
            "{target}"
        );
        assert!(fixture.receipt(target).is_err());
    }
    Ok(())
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
