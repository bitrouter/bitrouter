//! OpenClaw instance lifecycle and credential replicas, without real user state.
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const MODEL: &str = "bitrouter/automatos";
const REFERENCE: &str = "bitrouter/bitrouter/automatos";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        for name in ["bro", "claw/agents/main/agent", "home"] {
            std::fs::create_dir_all(root.join(name))?;
        }
        Ok(Self { _temp: temp, root })
    }
    fn config(&self) -> PathBuf {
        self.root.join("claw/openclaw.json")
    }
    fn catalog(&self) -> PathBuf {
        self.root.join("claw/agents/main/agent/models.json")
    }
    fn write(&self, value: &Value) -> Result<()> {
        Ok(std::fs::write(
            self.config(),
            serde_json::to_string_pretty(value)?,
        )?)
    }
    fn read(&self) -> Result<Value> {
        Ok(json5::from_str(&std::fs::read_to_string(self.config())?)?)
    }
    async fn run(&self, args: &[&str], token: bool, success: bool) -> Result<Value> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(["connect", "openclaw"])
            .args(args)
            .arg("--json")
            .env("HOME", self.root.join("home"))
            .env("BITROUTER_HOME", self.root.join("bro"))
            .env("OPENCLAW_STATE_DIR", self.root.join("claw"))
            .env_remove("OPENCLAW_CONFIG_PATH")
            .env_remove("OPENCLAW_AGENT_DIR")
            .env_remove("OPENCLAW_PROFILE")
            .env_remove("OPENCLAW_HOME")
            .env_remove("OPENCLAW_CONFIG_READONLY")
            .env_remove("OPENCLAW_NIX_MODE")
            .env_remove("BITROUTER_CONFIG")
            .env_remove("BITROUTER_CONTEXT")
            .env_remove("BITROUTER_API_KEY")
            .current_dir(&self.root)
            .kill_on_drop(true);
        if token {
            command.env("BITROUTER_API_KEY", "fixture-key");
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
            Ok(Value::String(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
    async fn connect(&self, server: &MockServer) -> Result<Value> {
        self.run(&["--base-url", &server.uri(), "--yes"], true, true)
            .await
    }
}

async fn gateway() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":MODEL},{"id":"team/next"}]})),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn json5_exact_wire_persisted_auth_and_scoped_removal() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = "{\n // retain this comment and single quotes\n agents: {defaults: {model: {primary:'native/first'}, modelPolicy: {allow: []}}},\n models: {mode:'merge', providers:{native:{baseUrl:'https://native.invalid/v1'}}},\n ui: {assistant:{name:'Original'}},\n}\n";
    std::fs::write(fixture.config(), original)?;
    std::fs::write(
        fixture.catalog(),
        "{\"providers\":{\"native\":{\"apiKey\":\"keep-key\"}}}\n",
    )?;
    let server = gateway().await;
    let result = fixture.connect(&server).await?;
    assert_eq!(result["model"], MODEL);
    assert_eq!(result["display_name"], "automatos");
    let text = std::fs::read_to_string(fixture.config())?;
    assert!(!text.contains("fixture-key"));
    assert!(text.contains("// retain this comment and single quotes"));
    assert!(text.contains("primary:'native/first'"));
    let mut value = fixture.read()?;
    assert_eq!(
        value["models"]["providers"]["bitrouter"]["models"][0]["id"],
        MODEL
    );
    assert_eq!(value["models"]["providers"]["bitrouter"]["auth"], "api-key");
    assert_eq!(
        value["models"]["providers"]["bitrouter"]["apiKey"],
        json!({"source":"file","provider":"bitrouter-connect","id":"/apiKey"})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(
            fixture.root.join("claw/bitrouter-connect-credential.json")
        )?)?["apiKey"],
        "fixture-key"
    );
    assert!(!std::fs::read_to_string(fixture.catalog())?.contains("fixture-key"));
    assert_eq!(
        value["agents"]["defaults"]["models"][REFERENCE]["alias"],
        "automatos"
    );
    fixture.run(&["--check"], false, true).await?;
    value["agents"]["defaults"]["model"]["primary"] = json!("bitrouter/automatos");
    value["ui"]["assistant"]["name"] = json!("Later preference");
    fixture.write(&value)?;
    let mut catalog: Value = serde_json::from_slice(&std::fs::read(fixture.catalog())?)?;
    catalog["providers"]["later"] = json!({"apiKey":"later-key"});
    std::fs::write(fixture.catalog(), serde_json::to_string_pretty(&catalog)?)?;
    assert_eq!(
        fixture.run(&["--status"], false, true).await?["configuration_verified"],
        true
    );
    fixture.run(&["--remove", "--yes"], false, true).await?;
    assert!(
        !fixture
            .root
            .join("claw/bitrouter-connect-credential.json")
            .exists()
    );
    let restored = fixture.read()?;
    assert!(restored.pointer("/models/providers/bitrouter").is_none());
    assert!(restored.pointer("/agents/defaults/models").is_none());
    assert_eq!(
        restored["agents"]["defaults"]["model"]["primary"],
        "native/first"
    );
    assert_eq!(restored["ui"]["assistant"]["name"], "Later preference");
    let catalog: Value = serde_json::from_slice(&std::fs::read(fixture.catalog())?)?;
    assert!(catalog["providers"].get("bitrouter").is_none());
    assert_eq!(catalog["providers"]["native"]["apiKey"], "keep-key");
    assert_eq!(catalog["providers"]["later"]["apiKey"], "later-key");
    Ok(())
}

#[tokio::test]
async fn policy_conflicts_name_agent_and_do_not_write() -> Result<()> {
    let server = gateway().await;
    for value in [
        json!({"agents":{"defaults":{"modelPolicy":{"allow":["native/*"]}}}}),
        json!({"agents":{"defaults":{"modelPolicy":{"allow":["bitrouter/*"]}},"entries":{"work":{"modelPolicy":{"allow":["native/first"]}}}}}),
        json!({"agents":{"defaults":{"models":{"native/first":{}}}}}),
    ] {
        let fixture = Fixture::new()?;
        fixture.write(&value)?;
        let original = std::fs::read(fixture.config())?;
        let error = fixture
            .run(&["--base-url", &server.uri(), "--yes"], true, false)
            .await?;
        assert!(error.as_str().context("error string")?.contains("excludes"));
        assert_eq!(std::fs::read(fixture.config())?, original);
        assert!(!fixture.catalog().exists());
    }
    Ok(())
}

#[tokio::test]
async fn includes_provider_alias_and_catalog_conflicts_fail_closed() -> Result<()> {
    let server = gateway().await;
    for value in [
        json!({"$include":"shared.json5"}),
        json!({"models":{"providers":{"BitRouter":{}}}}),
        json!({"agents":{"defaults":{"models":{"native/first":{"alias":"automatos"}}}}}),
    ] {
        let fixture = Fixture::new()?;
        fixture.write(&value)?;
        let original = std::fs::read(fixture.config())?;
        fixture
            .run(&["--base-url", &server.uri(), "--yes"], true, false)
            .await?;
        assert_eq!(std::fs::read(fixture.config())?, original);
    }
    let fixture = Fixture::new()?;
    std::fs::write(
        fixture.catalog(),
        "{providers:{bitrouter:{baseUrl:'https://old.invalid'}}}",
    )?;
    fixture
        .run(&["--base-url", &server.uri(), "--yes"], true, false)
        .await?;
    assert!(!fixture.config().exists());
    Ok(())
}

#[tokio::test]
async fn fresh_instance_dry_run_restart_and_exact_restore() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture
        .run(&["--base-url", &server.uri(), "--dry-run"], true, true)
        .await?;
    assert!(!fixture.config().exists());
    assert!(!fixture.catalog().exists());
    fixture.connect(&server).await?;
    assert_eq!(
        fixture.read()?["agents"]["defaults"]["modelPolicy"],
        json!({})
    );
    fixture.run(&["--status"], false, true).await?;
    fixture.run(&["--check"], false, true).await?;
    fixture.run(&["--remove", "--yes"], false, true).await?;
    assert!(!fixture.config().exists());
    assert!(!fixture.catalog().exists());
    Ok(())
}

#[tokio::test]
async fn edited_owned_provider_and_unrecorded_replica_block_removal() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture.connect(&server).await?;
    let original = std::fs::read(fixture.config())?;
    let replica = fixture.root.join("claw/agents/new/agent/models.json");
    std::fs::create_dir_all(replica.parent().context("replica parent")?)?;
    std::fs::copy(fixture.catalog(), &replica)?;
    let status = fixture.run(&["--status"], false, true).await?;
    assert_eq!(status["configuration_verified"], false);
    fixture.run(&["--remove", "--yes"], false, false).await?;
    assert_eq!(std::fs::read(fixture.config())?, original);
    std::fs::remove_file(replica)?;
    let mut value = fixture.read()?;
    value["models"]["providers"]["bitrouter"]["baseUrl"] = json!("https://edited.invalid/v1");
    fixture.write(&value)?;
    fixture.run(&["--remove", "--yes"], false, false).await?;
    assert!(
        fixture
            .read()?
            .pointer("/models/providers/bitrouter")
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn update_rotates_exact_model_and_retains_original_defaults() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture.write(
        &json!({"agents":{"defaults":{"model":{"primary":"native/original"},"modelPolicy":{}}}}),
    )?;
    fixture.connect(&server).await?;
    fixture
        .run(
            &[
                "--update",
                "--model",
                "team/next",
                "--display-name",
                "Next router",
                "--yes",
            ],
            true,
            true,
        )
        .await?;
    let value = fixture.read()?;
    assert_eq!(
        value["models"]["providers"]["bitrouter"]["models"][0]["id"],
        "team/next"
    );
    assert_eq!(
        value["agents"]["defaults"]["models"]["bitrouter/team/next"]["alias"],
        "automatos"
    );
    fixture.run(&["--remove", "--yes"], false, true).await?;
    assert_eq!(
        fixture.read()?["agents"]["defaults"]["model"]["primary"],
        "native/original"
    );
    Ok(())
}

#[tokio::test]
async fn unsupported_protocol_and_legacy_agent_shape_are_not_written() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture
        .run(
            &[
                "--base-url",
                &server.uri(),
                "--protocol",
                "responses",
                "--yes",
            ],
            true,
            false,
        )
        .await?;
    assert!(!fixture.config().exists());
    fixture.write(&json!({"agents":{"list":[{"id":"main"}]}}))?;
    fixture
        .run(&["--base-url", &server.uri(), "--yes"], true, false)
        .await?;
    assert!(!fixture.catalog().exists());
    Ok(())
}

#[tokio::test]
async fn segment_policy_wildcards_and_later_unrestricted_metadata_survive() -> Result<()> {
    let server = gateway().await;
    for allow in [
        "bitrouter/bitrouter/*",
        "bitrouter/*",
        "automatos",
        "bitrouter/automatos",
    ] {
        let fixture = Fixture::new()?;
        fixture.write(&json!({"agents":{"defaults":{"modelPolicy":{"allow":[allow]}}}}))?;
        fixture.connect(&server).await?;
        fixture.run(&["--remove", "--yes"], false, true).await?;
    }
    let fixture = Fixture::new()?;
    fixture.connect(&server).await?;
    let mut value = fixture.read()?;
    value["agents"]["defaults"]["models"]["native/later"] = json!({"alias":"later"});
    fixture.write(&value)?;
    fixture.run(&["--remove", "--yes"], false, true).await?;
    let restored = fixture.read()?;
    assert_eq!(restored["agents"]["defaults"]["modelPolicy"], json!({}));
    assert_eq!(
        restored["agents"]["defaults"]["models"]["native/later"]["alias"],
        "later"
    );
    Ok(())
}

#[tokio::test]
async fn split_config_and_state_are_rejected_without_catalog_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    let external = fixture.root.join("external.json5");
    std::fs::write(&external, "{}\n")?;
    fixture
        .run(
            &[
                "--instance",
                external.to_str().context("external path")?,
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            true,
            false,
        )
        .await?;
    assert_eq!(std::fs::read_to_string(external)?, "{}\n");
    assert!(!fixture.catalog().exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_agent_cannot_hide_unrecorded_credentials() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture.connect(&server).await?;
    let extra = fixture.root.join("external-agent");
    std::fs::create_dir_all(extra.join("agent"))?;
    std::fs::copy(fixture.catalog(), extra.join("agent/models.json"))?;
    std::os::unix::fs::symlink(&extra, fixture.root.join("claw/agents/linked"))?;
    fixture.run(&["--remove", "--yes"], false, false).await?;
    assert!(fixture.config().exists());
    assert!(extra.join("agent/models.json").exists());
    Ok(())
}

#[tokio::test]
async fn explicit_config_env_without_state_override_cannot_hide_native_state() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    let external = fixture.root.join("external.json5");
    std::fs::write(&external, "{}\n")?;
    let output = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args([
            "connect",
            "openclaw",
            "--instance",
            external.to_str().context("external path")?,
            "--base-url",
            &server.uri(),
            "--yes",
            "--json",
        ])
        .env("HOME", fixture.root.join("home"))
        .env("OPENCLAW_CONFIG_PATH", &external)
        .env("BITROUTER_HOME", fixture.root.join("bro"))
        .env("BITROUTER_API_KEY", "fixture-key")
        .env_remove("OPENCLAW_STATE_DIR")
        .env_remove("OPENCLAW_HOME")
        .env_remove("BITROUTER_CONFIG")
        .env_remove("BITROUTER_CONTEXT")
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("config and state directories differ")
    );
    assert_eq!(std::fs::read_to_string(external)?, "{}\n");
    Ok(())
}

#[tokio::test]
async fn native_alias_key_canonicalization_preserves_owned_wire_and_removal() -> Result<()> {
    let fixture = Fixture::new()?;
    let server = gateway().await;
    fixture.write(
        &json!({"agents":{"defaults":{"model":{"primary":"native/original"},"modelPolicy":{}}}}),
    )?;
    fixture.connect(&server).await?;
    let mut value = fixture.read()?;
    let models = value["agents"]["defaults"]["models"]
        .as_object_mut()
        .context("models")?;
    let alias = models.remove(REFERENCE).context("owned alias")?;
    models.insert(MODEL.into(), alias);
    value["agents"]["defaults"]["model"]["primary"] = json!(MODEL);
    fixture.write(&value)?;
    assert_eq!(
        fixture.run(&["--status"], false, true).await?["configuration_verified"],
        true
    );
    fixture.run(&["--remove", "--yes"], false, true).await?;
    let value = fixture.read()?;
    assert_eq!(
        value["agents"]["defaults"]["model"]["primary"],
        "native/original"
    );
    assert!(value.pointer("/agents/defaults/models").is_none());
    Ok(())
}

#[tokio::test]
async fn later_shared_secret_references_block_revoking_the_private_file() -> Result<()> {
    let server = gateway().await;
    for main_config in [true, false] {
        let fixture = Fixture::new()?;
        fixture.connect(&server).await?;
        let reference = json!({"source":"file","provider":"bitrouter-connect","id":"/apiKey"});
        if main_config {
            let mut value = fixture.read()?;
            value["models"]["providers"]["later"] = json!({"apiKey":reference});
            fixture.write(&value)?;
        } else {
            let mut value: Value = serde_json::from_slice(&std::fs::read(fixture.catalog())?)?;
            value["providers"]["later"] = json!({"apiKey":reference});
            std::fs::write(fixture.catalog(), serde_json::to_string_pretty(&value)?)?;
        }
        fixture.run(&["--remove", "--yes"], false, false).await?;
        assert!(
            fixture
                .root
                .join("claw/bitrouter-connect-credential.json")
                .exists()
        );
        assert!(
            fixture
                .read()?
                .pointer("/models/providers/bitrouter")
                .is_some()
        );
    }
    Ok(())
}
