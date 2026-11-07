//! Isolated native Harness configuration lifecycle. Runtime acceptance is documented separately.
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
    home: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let home = root.join("harness");
        std::fs::create_dir_all(home.join("profiles"))?;
        Ok(Self {
            _temp: temp,
            root,
            home,
        })
    }

    fn profile(&self, name: &str, patch: &str) -> Result<PathBuf> {
        let root = self.home.join("profiles").join(name);
        std::fs::create_dir_all(&root)?;
        let bundle = match name {
            "web" | "desktop" => "@deepseek-ai/dsh-web-app",
            "headless" => "@deepseek-ai/dsh-headless",
            _ => "@deepseek-ai/dsh-sdk-app",
        };
        std::fs::write(
            root.join("package.json"),
            json!({"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base",bundle]}}}).to_string(),
        )?;
        let path = root.join("cordis.patch.yml");
        std::fs::write(&path, patch)?;
        Ok(path)
    }

    async fn run(&self, arguments: &[&str], success: bool) -> Result<Value> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("BITROUTER_HOME", self.root.join("bro"))
            .env("BITROUTER_API_KEY", "isolated-harness-key")
            .env("DSH_HOME", &self.home)
            .current_dir(&self.root)
            .args(arguments)
            .kill_on_drop(true);
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(20), command.output()).await??;
        assert_eq!(
            output.status.success(),
            success,
            "{arguments:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("isolated-harness-key"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("isolated-harness-key"));
        if success {
            Ok(serde_json::from_slice(&output.stdout)?)
        } else {
            Ok(Value::Null)
        }
    }
}

fn yaml(path: &Path) -> Result<Value> {
    Ok(serde_saphyr::from_str(&std::fs::read_to_string(path)?)?)
}
fn row<'a>(document: &'a Value, id: &str) -> Result<&'a Value> {
    document
        .as_array()
        .context("patch array")?
        .iter()
        .rev()
        .find(|row| row["id"] == id)
        .context("missing row")
}

async fn gateway() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer isolated-harness-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"bitrouter/automatos"}]})),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn initialized_surfaces_gain_independent_provider_and_restore_picker_preferences()
-> Result<()> {
    let fixture = Fixture::new()?;
    let original = "# retained profile header\n- id: llm-pi-ai\n  config:\n    providers:\n      existing:\n        api: openai-completions\n        baseURL: https://native.example/v1\n        models: [{id: native}]\n- id: agent-default-model\n  config: {provider: existing, model: native}\n- id: unrelated\n  config: {value: keep}\n";
    let mut paths = vec![];
    for name in ["web", "headless", "desktop"] {
        paths.push(fixture.profile(name, original)?);
    }
    std::fs::create_dir_all(fixture.home.join("profiles/uninitialized"))?;
    let server = gateway().await;
    let report = fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            true,
        )
        .await?;
    assert_eq!(report["gateway_catalog_verified"], true);
    assert_eq!(report["inference_verified"], false);
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--profile",
                "web",
                "--remove",
                "--yes",
            ],
            false,
        )
        .await?;
    // Profile scope is a set, independent of flag order or duplicates.
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--profile",
                "web",
                "--profile",
                "headless",
                "--profile",
                "desktop",
                "--profile",
                "web",
                "--yes",
            ],
            true,
        )
        .await?;
    for path in &paths {
        let value = yaml(path)?;
        let provider = &row(&value, "llm-pi-ai")?["config"]["providers"];
        assert_eq!(provider["existing"]["models"][0]["id"], "native");
        assert_eq!(
            provider["bitrouter"]["models"][0]["id"],
            "bitrouter/automatos"
        );
        assert_eq!(
            provider["bitrouter"]["models"][0]["name"],
            "bitrouter/automatos"
        );
        assert_eq!(provider["bitrouter"]["api"], "openai-completions");
        assert_eq!(
            row(&value, "agent-default-model")?["config"]["provider"],
            "existing"
        );
        assert_eq!(row(&value, "credentials")?["disabled"], false);
        // Native picker makes the chosen provider the default; another settings edit occurs too.
        let mut value = value;
        let rows = value.as_array_mut().context("rows")?;
        for row in rows.iter_mut() {
            if row["id"] == "agent-default-model" {
                row["config"] = json!({"provider":"bitrouter","model":"bitrouter/automatos"});
            }
            if row["id"] == "unrelated" {
                row["config"]["later"] = json!(true);
            }
        }
        rows.push(json!({"id":"new-preference","config":{"theme":"dark"}}));
        std::fs::write(path, serde_saphyr::to_string(&value)?)?;
    }
    assert!(
        !fixture
            .home
            .join("profiles/uninitialized/cordis.patch.yml")
            .exists()
    );
    fixture
        .run(&["connect", "deepseek-harness", "--yes"], true)
        .await?;
    fixture
        .run(&["connect", "deepseek-harness", "--remove", "--yes"], true)
        .await?;
    for path in paths {
        let value = yaml(&path)?;
        assert!(row(&value, "llm-pi-ai")?["config"]["providers"]["bitrouter"].is_null());
        assert_eq!(
            row(&value, "agent-default-model")?["config"]["model"],
            "native"
        );
        assert_eq!(row(&value, "new-preference")?["config"]["theme"], "dark");
        assert_eq!(row(&value, "unrelated")?["config"]["later"], true);
    }
    assert!(!fixture.home.join(".credentials.yaml").exists());
    Ok(())
}

#[tokio::test]
async fn explicit_profile_dry_run_protocol_and_exact_instance_scope() -> Result<()> {
    let fixture = Fixture::new()?;
    let web = fixture.profile("web", "[]\n")?;
    let desktop = fixture.profile("desktop", "[]\n")?;
    let endpoint = "http://127.0.0.1:4356";
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                endpoint,
                "--profile",
                "web",
                "--dry-run",
                "--yes",
            ],
            true,
        )
        .await?;
    assert_eq!(std::fs::read_to_string(&web)?, "[]\n");
    assert!(!fixture.home.join(".credentials.yaml").exists());
    let server = gateway().await;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--instance",
                fixture.home.to_str().context("home")?,
                "--profile",
                "web",
                "--protocol",
                "responses",
                "--yes",
            ],
            true,
        )
        .await?;
    assert_eq!(
        row(&yaml(&web)?, "llm-pi-ai")?["config"]["providers"]["bitrouter"]["api"],
        "openai-responses"
    );
    assert_eq!(std::fs::read_to_string(&desktop)?, "[]\n");
    fixture
        .run(&["connect", "deepseek-harness", "--remove", "--yes"], true)
        .await?;
    assert_eq!(std::fs::read_to_string(&web)?, "[]\n");
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--profile",
                "web",
                "--protocol",
                "messages",
                "--yes",
            ],
            true,
        )
        .await?;
    assert_eq!(
        row(&yaml(&web)?, "llm-pi-ai")?["config"]["providers"]["bitrouter"]["baseURL"],
        server.uri()
    );
    fixture
        .run(&["connect", "deepseek-harness", "--remove", "--yes"], true)
        .await?;
    Ok(())
}

#[tokio::test]
async fn later_profile_reference_preserves_shared_key_and_other_credentials() -> Result<()> {
    let fixture = Fixture::new()?;
    let web = fixture.profile("web", "[]\n")?;
    let credential_path = fixture.home.join(".credentials.yaml");
    let prior = json!({"version":1,"refs":{"EXISTING_API_KEY":"native"},"records":{"llm-pi-ai/native":{"kind":"api-key","apiKey":"native"}},"foreign":{"retained":true}});
    std::fs::write(&credential_path, serde_saphyr::to_string(&prior)?)?;
    let server = gateway().await;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            true,
        )
        .await?;
    let provider = row(&yaml(&web)?, "llm-pi-ai")?["config"]["providers"]["bitrouter"].clone();
    fixture.profile(
        "later",
        &serde_saphyr::to_string(
            &json!([{"id":"llm-pi-ai","config":{"providers":{"copied":provider}}}]),
        )?,
    )?;
    fixture
        .run(&["connect", "deepseek-harness", "--remove", "--yes"], true)
        .await?;
    let credentials = yaml(&credential_path)?;
    assert_eq!(
        credentials["refs"]["BITROUTER_CONNECT_API_KEY"],
        "isolated-harness-key"
    );
    assert_eq!(credentials["refs"]["EXISTING_API_KEY"], "native");
    assert_eq!(credentials["foreign"], prior["foreign"]);
    Ok(())
}

#[tokio::test]
async fn rejects_precedence_collisions_missing_profiles_and_modified_owned_provider() -> Result<()>
{
    let fixture = Fixture::new()?;
    let web = fixture.profile("web", "[]\n")?;
    let server = gateway().await;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--profile",
                "missing",
                "--yes",
            ],
            false,
        )
        .await?;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--profile",
                "../web",
                "--yes",
            ],
            false,
        )
        .await?;
    std::fs::write(
        fixture.home.join("cordis.patch.yml"),
        "- id: llm-pi-ai\n  config: {providers: {}}\n",
    )?;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            false,
        )
        .await?;
    assert_eq!(std::fs::read_to_string(&web)?, "[]\n");
    std::fs::remove_file(fixture.home.join("cordis.patch.yml"))?;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            true,
        )
        .await?;
    let mut value = yaml(&web)?;
    for row in value.as_array_mut().context("rows")? {
        if row["id"] == "llm-pi-ai" {
            row["config"]["providers"]["bitrouter"]["baseURL"] =
                json!("https://changed.example/v1");
        }
    }
    let changed = serde_saphyr::to_string(&value)?;
    std::fs::write(&web, &changed)?;
    fixture
        .run(&["connect", "deepseek-harness", "--remove", "--yes"], false)
        .await?;
    assert_eq!(std::fs::read_to_string(&web)?, changed);
    assert!(fixture.home.join(".credentials.yaml").exists());
    Ok(())
}

#[tokio::test]
async fn interrupted_restore_and_dynamic_unrelated_yaml_remain_safe() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = "# fixture\n- id: unrelated\n  config:\n    dynamic: !!js process.cwd()\n";
    let web = fixture.profile("web", original)?;
    let server = gateway().await;
    fixture
        .run(
            &[
                "connect",
                "deepseek-harness",
                "--base-url",
                &server.uri(),
                "--yes",
            ],
            true,
        )
        .await?;
    assert!(std::fs::read_to_string(&web)?.starts_with(original));
    // A previous restore reached this file but not credentials/receipt deletion.
    std::fs::write(&web, original)?;
    fixture
        .run(&["connect", "deepseek-harness", "--remove", "--yes"], true)
        .await?;
    assert_eq!(std::fs::read_to_string(&web)?, original);
    assert!(!fixture.home.join(".credentials.yaml").exists());
    Ok(())
}
