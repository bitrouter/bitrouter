//! Hermes native configuration transactions use isolated profile homes.
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
        for directory in [
            "bro",
            "hermes/profiles/work",
            "hermes/profiles/other",
            "second",
        ] {
            std::fs::create_dir_all(root.join(directory))?;
        }
        for profile in ["hermes", "hermes/profiles/work", "hermes/profiles/other"] {
            std::fs::write(
                root.join(profile).join("config.yaml"),
                "# native comment\nversion: 12\nmodel:\n  provider: native\n  default: native-first\n  base_url: https://native.invalid/v1\nproviders:\n  native: # provider comment\n    api: https://native.invalid/v1\nauxiliary:\n  compression:\n    provider: native\n    model: native-small\n",
            )?;
        }
        Ok(Self { _temp: temp, root })
    }
    async fn run(&self, args: &[&str], success: bool) -> Result<Value> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            Command::new(env!("CARGO_BIN_EXE_bro"))
                .args(args)
                .current_dir(&self.root)
                .env("BITROUTER_HOME", self.root.join("bro"))
                .env("BITROUTER_API_KEY", "fixture-key")
                .env("HERMES_HOME", self.root.join("hermes"))
                .env_remove("BITROUTER_CONFIG")
                .env_remove("BITROUTER_CONTEXT")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
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
    fn yaml(&self, profile: &str) -> Result<Value> {
        Ok(serde_saphyr::from_str(&std::fs::read_to_string(
            self.root.join(profile).join("config.yaml"),
        )?)?)
    }
    fn write(&self, profile: &str, value: &Value) -> Result<()> {
        Ok(std::fs::write(
            self.root.join(profile).join("config.yaml"),
            serde_saphyr::to_string(value)?,
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
async fn hermes_provider_preserves_defaults_and_exact_gateway_model() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = "team/automatos-wire";
    let server = gateway(model).await;
    let url = format!("{}/proxy/v1/", server.uri());
    let report = fixture
        .run(
            &[
                "connect",
                "hermes",
                "--base-url",
                &url,
                "--model",
                model,
                "--display-name",
                "automatos",
                "--yes",
            ],
            true,
        )
        .await?;
    assert_eq!(report["target"], "hermes");
    assert_eq!(report["model"], model);
    assert_eq!(report["display_name"], "automatos");
    assert_eq!(report["gateway_catalog_verified"], true);
    assert_eq!(report["inference_verified"], false);
    let mut value = fixture.yaml("hermes")?;
    let provider = &value["providers"]["bitrouter"];
    assert_eq!(provider["name"], "BitRouter");
    assert_eq!(provider["api"], format!("{}/proxy/v1", server.uri()));
    assert_eq!(provider["transport"], "chat_completions");
    assert_eq!(provider["discover_models"], false);
    assert_eq!(provider["models"], json!({model:{}}));
    assert_eq!(value["model"]["default"], "native-first");
    assert_eq!(value["auxiliary"]["compression"]["model"], "native-small");
    let text = std::fs::read_to_string(fixture.root.join("hermes/config.yaml"))?;
    assert!(text.contains("# native comment"));
    assert!(text.contains("native: # provider comment"));
    value["providers"]["later"] = json!({"api":"https://later.invalid/v1"});
    value["model"]["default"] = json!("native-later");
    value["display"] = json!({"theme":"light"});
    fixture.write("hermes", &value)?;
    for args in [
        ["connect", "hermes", "--yes"],
        ["connect", "hermes", "--status"],
        ["connect", "hermes", "--check"],
    ] {
        fixture.run(&args, true).await?;
    }
    fixture
        .run(&["connect", "hermes", "--remove", "--yes"], true)
        .await?;
    let value = fixture.yaml("hermes")?;
    assert!(value["providers"].get("bitrouter").is_none());
    assert!(value["providers"].get("native").is_some());
    assert!(value["providers"].get("later").is_some());
    assert_eq!(value["model"]["default"], "native-later");
    assert_eq!(value["display"]["theme"], "light");
    assert_eq!(value["auxiliary"]["compression"]["model"], "native-small");
    assert_eq!(
        server
            .received_requests()
            .await
            .context("missing requests")?
            .len(),
        2,
        "setup and --check only; no inference"
    );
    Ok(())
}

#[tokio::test]
async fn hermes_explicit_profiles_normalize_and_owned_selection_recovers() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = "bitrouter/automatos";
    let server = gateway(model).await;
    let url = format!("{}/proxy", server.uri());
    let root_before = std::fs::read(fixture.root.join("hermes/config.yaml"))?;
    let other_before = std::fs::read(fixture.root.join("hermes/profiles/other/config.yaml"))?;
    fixture
        .run(
            &[
                "connect",
                "hermes",
                "--profile",
                " Work ",
                "--base-url",
                &url,
                "--yes",
            ],
            true,
        )
        .await?;
    fixture
        .run(&["connect", "hermes", "--profile", "Work", "--yes"], true)
        .await?;
    assert_eq!(
        std::fs::read(fixture.root.join("hermes/config.yaml"))?,
        root_before
    );
    assert_eq!(
        std::fs::read(fixture.root.join("hermes/profiles/other/config.yaml"))?,
        other_before
    );
    let mut value = fixture.yaml("hermes/profiles/work")?;
    assert!(value["providers"].get("bitrouter").is_some());
    value["model"]["provider"] = json!("bitrouter");
    value["model"]["default"] = json!(model);
    value["model"]["base_url"] = json!(format!("{url}/v1"));
    value["model"]["api_mode"] = json!("chat_completions");
    value["model"]["reasoning_effort"] = json!("high");
    fixture.write("hermes/profiles/work", &value)?;
    fixture
        .run(
            &[
                "connect",
                "hermes",
                "--profile",
                "WORK",
                "--remove",
                "--yes",
            ],
            true,
        )
        .await?;
    let value = fixture.yaml("hermes/profiles/work")?;
    assert_eq!(value["model"]["provider"], "native");
    assert_eq!(value["model"]["default"], "native-first");
    assert_eq!(value["model"]["base_url"], "https://native.invalid/v1");
    assert_eq!(value["model"]["reasoning_effort"], "high");
    assert!(value["model"].get("api_mode").is_none());
    assert!(value["providers"].get("bitrouter").is_none());
    Ok(())
}

#[tokio::test]
async fn hermes_edited_provider_blocks_removal_and_unverified_protocols_are_rejected() -> Result<()>
{
    let fixture = Fixture::new()?;
    let server = gateway("bitrouter/automatos").await;
    let url = format!("{}/proxy", server.uri());
    for protocol in ["responses", "messages"] {
        fixture
            .run(
                &[
                    "connect",
                    "hermes",
                    "--base-url",
                    &url,
                    "--protocol",
                    protocol,
                    "--yes",
                ],
                false,
            )
            .await?;
    }
    fixture
        .run(&["connect", "hermes", "--base-url", &url, "--yes"], true)
        .await?;
    let mut value = fixture.yaml("hermes")?;
    value["providers"]["bitrouter"]["api_key"] = json!("native-edit");
    fixture.write("hermes", &value)?;
    fixture
        .run(&["connect", "hermes", "--remove", "--yes"], false)
        .await?;
    assert_eq!(
        fixture.yaml("hermes")?["providers"]["bitrouter"]["api_key"],
        "native-edit"
    );
    Ok(())
}
