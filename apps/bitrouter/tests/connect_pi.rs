//! Persistent Pi CLI/TUI configuration, isolated from the user's real profile.
use std::path::PathBuf;

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
        for directory in ["bro", "agent", "second-agent"] {
            std::fs::create_dir_all(root.join(directory))?;
        }
        std::fs::write(
            root.join("agent/models.json"),
            "{\"providers\":{\"native\":{\"baseUrl\":\"https://native.invalid/v1\"}}}",
        )?;
        std::fs::write(
            root.join("agent/auth.json"),
            "{\"native\":{\"type\":\"api_key\",\"key\":\"native-key\"}}",
        )?;
        std::fs::write(
            root.join("agent/settings.json"),
            "{\"defaultProvider\":\"native\",\"defaultModel\":\"native-first\",\"theme\":\"dark\"}\n",
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
            .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
            .env_remove("BITROUTER_CONFIG")
            .env_remove("BITROUTER_CONTEXT")
            .kill_on_drop(true);
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

    fn json(&self, name: &str) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.root.join(name),
        )?)?)
    }

    fn write_json(&self, name: &str, value: &Value) -> Result<()> {
        Ok(std::fs::write(
            self.root.join(name),
            serde_json::to_string_pretty(value)?,
        )?)
    }
}

async fn gateway(model: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/proxy/v1/models"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":model}]})))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn persisted_provider_exact_model_status_and_native_edits_survive_removal() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = "team/automatos-wire";
    let server = gateway(model).await;
    let url = format!("{}/proxy/v1/", server.uri());
    let before_settings = std::fs::read(fixture.root.join("agent/settings.json"))?;
    let report = fixture
        .run(
            &[
                "connect",
                "pi",
                "--base-url",
                &url,
                "--model",
                model,
                "--display-name",
                "bitrouter/automatos",
                "--yes",
            ],
            true,
        )
        .await?;
    assert_eq!(report["target"], "pi");
    assert_eq!(report["model"], model);
    assert_eq!(report["display_name"], "bitrouter/automatos");
    assert_eq!(report["gateway_catalog_verified"], true);
    assert_eq!(report["inference_verified"], false);
    assert_eq!(
        std::fs::read(fixture.root.join("agent/settings.json"))?,
        before_settings
    );
    let mut models = fixture.json("agent/models.json")?;
    assert_eq!(
        models["providers"]["bitrouter"]["baseUrl"],
        format!("{}/proxy/v1", server.uri())
    );
    assert_eq!(
        models["providers"]["bitrouter"]["models"][0],
        json!({"id":model,"name":"bitrouter/automatos"})
    );
    assert_eq!(
        fixture.json("agent/auth.json")?["bitrouter"],
        json!({"type":"api_key","key":"fixture-key"})
    );
    models["providers"]["later-native"] = json!({"models":[{"id":"later"}]});
    fixture.write_json("agent/models.json", &models)?;
    fixture.write_json(
        "agent/settings.json",
        &json!({"defaultProvider":"native","defaultModel":"native-later","theme":"light"}),
    )?;
    for args in [["connect", "pi", "--yes"], ["connect", "pi", "--status"]] {
        let report = fixture.run(&args, true).await?;
        assert_eq!(report["model"], model);
        assert_eq!(report["inference_verified"], false);
    }
    fixture
        .run(&["connect", "pi", "--remove", "--yes"], true)
        .await?;
    let models = fixture.json("agent/models.json")?;
    assert!(models["providers"].get("bitrouter").is_none());
    assert!(models["providers"].get("native").is_some());
    assert!(models["providers"].get("later-native").is_some());
    assert!(fixture.json("agent/auth.json")?.get("bitrouter").is_none());
    assert_eq!(
        fixture.json("agent/settings.json")?,
        json!({"defaultProvider":"native","defaultModel":"native-later","theme":"light"})
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .context("missing fixture requests")?
            .len(),
        1,
        "status, repeat and removal must not make inference or catalog requests"
    );
    Ok(())
}

#[tokio::test]
async fn explicit_instances_are_independent_and_connected_default_is_recovered() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = "bitrouter/automatos";
    let server = gateway(model).await;
    let url = format!("{}/proxy", server.uri());
    let second = fixture.root.join("second-agent");
    let second = second.to_str().context("non-UTF8 instance")?;
    for instance in [
        fixture.root.join("agent"),
        fixture.root.join("second-agent"),
    ] {
        let instance = instance.to_str().context("non-UTF8 instance")?;
        fixture
            .run(
                &[
                    "connect",
                    "pi",
                    "--instance",
                    instance,
                    "--base-url",
                    &url,
                    "--protocol",
                    "responses",
                    "--yes",
                ],
                true,
            )
            .await?;
    }
    assert_eq!(
        fixture.json("second-agent/models.json")?["providers"]["bitrouter"]["api"],
        "openai-responses"
    );
    fixture.write_json(
        "agent/settings.json",
        &json!({"defaultProvider":"bitrouter","defaultModel":model,"theme":"light"}),
    )?;
    fixture
        .run(&["connect", "pi", "--remove", "--yes"], true)
        .await?;
    assert_eq!(
        fixture.json("agent/settings.json")?,
        json!({"defaultProvider":"native","defaultModel":"native-first","theme":"light"})
    );
    assert!(
        fixture.json("second-agent/models.json")?["providers"]
            .get("bitrouter")
            .is_some()
    );
    fixture
        .run(&["connect", "pi", "--instance", second, "--status"], true)
        .await?;
    fixture
        .run(
            &["connect", "pi", "--instance", second, "--remove", "--yes"],
            true,
        )
        .await?;
    for name in ["models.json", "auth.json", "settings.json"] {
        assert!(!fixture.root.join("second-agent").join(name).exists());
    }
    Ok(())
}

#[tokio::test]
async fn preview_missing_model_and_owned_edits_never_clobber_files() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway("bitrouter/automatos").await;
    let url = format!("{}/proxy", server.uri());
    let original = std::fs::read(fixture.root.join("agent/models.json"))?;
    fixture
        .run(
            &[
                "connect",
                "pi",
                "--base-url",
                "https://offline.invalid",
                "--dry-run",
            ],
            true,
        )
        .await?;
    fixture
        .run(
            &[
                "connect",
                "pi",
                "--base-url",
                &url,
                "--model",
                "absent/model",
                "--yes",
            ],
            false,
        )
        .await?;
    assert_eq!(
        std::fs::read(fixture.root.join("agent/models.json"))?,
        original
    );
    fixture
        .run(&["connect", "pi", "--base-url", &url, "--yes"], true)
        .await?;
    let mut auth = fixture.json("agent/auth.json")?;
    auth["bitrouter"]["key"] = json!("later-owned-edit");
    fixture.write_json("agent/auth.json", &auth)?;
    let models = std::fs::read(fixture.root.join("agent/models.json"))?;
    fixture
        .run(&["connect", "pi", "--remove", "--yes"], false)
        .await?;
    assert_eq!(
        std::fs::read(fixture.root.join("agent/models.json"))?,
        models
    );
    assert_eq!(fixture.json("agent/auth.json")?, auth);
    Ok(())
}
