//! Native client connection setup must also work in a gateway-only build.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        for directory in ["bro", "codex", "claude", "desktop/configLibrary"] {
            std::fs::create_dir_all(root.join(directory))?;
        }
        // Explicit remote setup must never parse this invalid local config.
        std::fs::write(root.join("bitrouter.yaml"), "[invalid: local configuration")?;
        std::fs::write(
            root.join("bro/bitrouter.yaml"),
            "[invalid: local configuration",
        )?;
        std::fs::write(
            root.join("codex/config.toml"),
            "# preserve\nmodel = 'native'\n",
        )?;
        std::fs::write(
            root.join("codex/models_cache.json"),
            json!({"models": [{
                "slug": "native", "display_name": "Native", "shell_type": "shell_command",
                "supported_reasoning_levels": [], "context_window": 8192
            }]})
            .to_string(),
        )?;
        std::fs::write(root.join("claude/settings.json"), "{\"model\":\"sonnet\"}")?;
        std::fs::write(
            root.join("desktop/configLibrary/_meta.json"),
            "{\"entries\":[]}",
        )?;
        Ok(Self { _temp: temp, root })
    }

    async fn run(&self, args: &[&str], success: bool) -> Result<Value> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(args)
            .current_dir(&self.root)
            .env("BITROUTER_HOME", self.root.join("bro"))
            .env("BITROUTER_API_KEY", "fixture-key")
            .env("CODEX_HOME", self.root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CLAUDE_USER_DATA_DIR", self.root.join("desktop"))
            .kill_on_drop(true);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("ANTHROPIC_")
                || name.starts_with("CLAUDE_CODE_")
                || name == "BITROUTER_CONTEXT"
                || name == "BITROUTER_CONFIG"
            {
                command.env_remove(key);
            }
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

    fn json(&self, path: &str) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.root.join(path),
        )?)?)
    }
}

async fn gateway(model: &str, prefix: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{prefix}/v1/models")))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": model}]})))
        .mount(&server)
        .await;
    server
}

fn client_paths(root: &Path) -> [PathBuf; 3] {
    [
        root.join("codex/config.toml"),
        root.join("claude/settings.json"),
        root.join("desktop/configLibrary/_meta.json"),
    ]
}

#[tokio::test]
async fn custom_gateway_model_connects_both_clients_without_local_configuration() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = "bitrouter/claude-automatos";
    let server = gateway(model, "/proxy/team").await;
    let endpoint = format!("{}/proxy/team", server.uri());
    let input_url = format!("{endpoint}/v1/");
    let original = client_paths(&fixture.root).map(std::fs::read);
    for target in ["codex", "claude"] {
        let args = [
            "connect",
            target,
            "--base-url",
            &input_url,
            "--model",
            model,
            "--display-name",
            "bitrouter/automatos",
            "--yes",
        ];
        let report = fixture.run(&args, true).await?;
        assert_eq!(report["state"], "configured_restart_required");
        assert_eq!(report["model"], model);
        assert_eq!(report["display_name"], "bitrouter/automatos");
        assert_eq!(report["endpoint"], endpoint);
        assert_eq!(report["gateway_catalog_verified"], true);
        assert_eq!(report["inference_verified"], false);
        // Omitting the options must report the stored custom model, not the default.
        let again = fixture.run(&["connect", target, "--yes"], true).await?;
        assert_eq!(again["state"], "already_configured_restart_required");
        assert_eq!(again["model"], model);
        assert_eq!(again["display_name"], "bitrouter/automatos");
        fixture
            .run(
                &[
                    "connect",
                    target,
                    "--display-name",
                    "Different label",
                    "--yes",
                ],
                false,
            )
            .await?;
        fixture
            .run(
                &["connect", target, "--model", "different/model", "--yes"],
                false,
            )
            .await?;
        fixture
            .run(
                &[
                    "connect",
                    target,
                    "--base-url",
                    "https://different.example",
                    "--yes",
                ],
                false,
            )
            .await?;
    }
    let models = fixture.json("codex/bitrouter-models.json")?;
    assert_eq!(models["models"][1]["slug"], model);
    assert_eq!(models["models"][1]["display_name"], "bitrouter/automatos");
    let settings = fixture.json("claude/settings.json")?;
    assert_eq!(settings["env"]["ANTHROPIC_CUSTOM_MODEL_OPTION"], model);
    assert_eq!(
        settings["env"]["ANTHROPIC_CUSTOM_MODEL_OPTION_NAME"],
        "bitrouter/automatos"
    );
    assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], endpoint);
    let profile =
        fixture.json("desktop/configLibrary/6c00b780-eab9-4f8f-a51e-4fc1c363b8cd.json")?;
    assert_eq!(profile["inferenceModels"][0]["name"], model);
    assert_eq!(
        profile["inferenceModels"][0]["labelOverride"],
        "bitrouter/automatos"
    );
    assert_eq!(profile["inferenceGatewayBaseUrl"], endpoint);
    for target in ["codex", "claude"] {
        let report = fixture
            .run(&["connect", target, "--remove", "--yes"], true)
            .await?;
        assert_eq!(report["state"], "restored");
        assert_eq!(report["model"], model);
    }
    for (path, before) in client_paths(&fixture.root).into_iter().zip(original) {
        assert_eq!(std::fs::read(path)?, before?);
    }
    assert!(!fixture.root.join("codex/bitrouter-models.json").exists());
    let requests = server
        .received_requests()
        .await
        .context("missing requests")?;
    assert_eq!(
        requests.len(),
        2,
        "setup must only verify the model catalog"
    );
    Ok(())
}

#[tokio::test]
async fn absent_models_and_dry_runs_do_not_change_client_files() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway("another/model", "").await;
    for target in ["codex", "claude"] {
        fixture
            .run(
                &[
                    "connect",
                    target,
                    "--base-url",
                    &server.uri(),
                    "--model",
                    "missing/model",
                    "--yes",
                ],
                false,
            )
            .await?;
        assert!(
            !fixture
                .root
                .join(format!("bro/connections/{target}.json"))
                .exists()
        );
        let preview = fixture
            .run(
                &[
                    "connect",
                    target,
                    "--base-url",
                    "https://offline.invalid",
                    "--dry-run",
                ],
                true,
            )
            .await?;
        assert_eq!(preview["state"], "preview");
        assert_eq!(preview["model"], "bitrouter/automatos");
    }
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("codex/config.toml"))?,
        "# preserve\nmodel = 'native'\n"
    );
    assert_eq!(
        fixture.json("claude/settings.json")?,
        json!({"model":"sonnet"})
    );
    assert_eq!(
        fixture.json("desktop/configLibrary/_meta.json")?,
        json!({"entries":[]})
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .context("missing requests")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn default_model_and_legacy_receipt_remain_usable() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway("bitrouter/automatos", "").await;
    let report = fixture
        .run(
            &["connect", "codex", "--base-url", &server.uri(), "--yes"],
            true,
        )
        .await?;
    assert_eq!(report["model"], "bitrouter/automatos");
    let receipt_path = fixture.root.join("bro/connections/codex.json");
    let mut receipt: Value = serde_json::from_slice(&std::fs::read(&receipt_path)?)?;
    receipt["version"] = json!(1);
    receipt
        .as_object_mut()
        .context("invalid receipt")?
        .remove("model");
    std::fs::write(receipt_path, receipt.to_string())?;
    let report = fixture
        .run(&["connect", "codex", "--remove", "--yes"], true)
        .await?;
    assert_eq!(report["model"], "bitrouter/automatos");
    assert_eq!(report["state"], "restored");
    Ok(())
}
