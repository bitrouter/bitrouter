use super::*;
use futures::StreamExt;
use std::{
    fs::File,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[path = "../support/prebuilt_example.rs"]
mod prebuilt_example;

struct Process(Child, PathBuf);
impl Drop for Process {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            if let Err(error) = self.0.kill() {
                eprintln!("fixture kill: {error}");
            }
            if let Err(error) = self.0.wait() {
                eprintln!("fixture reap: {error}");
            }
        }
    }
}
fn binary() -> Result<PathBuf> {
    if let Some(path) =
        prebuilt_example::from_env("routing_topology_fixture").map_err(anyhow::Error::msg)?
    {
        return Ok(path);
    }
    let executable = std::env::current_exe()?;
    let profile = executable
        .parent()
        .and_then(Path::parent)
        .context("Cargo profile")?;
    let target = profile.parent().context("Cargo target")?;
    let mut features = Vec::new();
    if cfg!(feature = "semantic-routing") {
        features.push("semantic-routing");
    }
    if cfg!(feature = "inference-routing") {
        features.push("inference-routing");
    }
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "build",
            "--locked",
            "-p",
            "bitrouter",
            "--no-default-features",
            "--features",
            &features.join(","),
            "--example",
            "routing_topology_fixture",
            "--target-dir",
        ])
        .arg(target);
    if profile.file_name().is_some_and(|name| name != "debug") {
        command
            .arg("--profile")
            .arg(profile.file_name().context("profile name")?);
    }
    ensure!(command.status()?.success(), "build topology fixture");
    Ok(profile.join("examples").join(format!(
        "routing_topology_fixture{}",
        std::env::consts::EXE_SUFFIX
    )))
}
fn address() -> Result<String> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.to_string())
}
fn spawn(binary: &Path, mode: &str, address: &str, path: &Path, home: &Path) -> Result<Process> {
    std::fs::create_dir_all(home)?;
    let log = home.join("stderr.log");
    let mut command = Command::new(binary);
    command.args([mode, address]).arg(path).env_clear();
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let child = command
        .env("HOME", home)
        .env("BITROUTER_HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&log)?)
        .spawn()?;
    Ok(Process(child, log))
}
async fn ready(process: &mut Process, address: &str, client: &reqwest::Client) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            ensure!(
                process.0.try_wait()?.is_none(),
                "fixture exited: {}",
                std::fs::read_to_string(&process.1)?
            );
            if client
                .get(format!("http://{address}/health"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("fixture readiness")?
}
fn journal(path: &Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    text.lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}
fn save(path: &Path, value: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_vec(value)?)?;
    Ok(())
}

fn capabilities(binary: &Path) -> Result<Value> {
    let output = Command::new(binary).arg("capabilities").output()?;
    ensure!(
        output.status.success(),
        "capabilities: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[tokio::test]
#[ignore = "requires separately prebuilt semantic-only and inference-only gateway fixtures"]
async fn heterogeneous_gateways_keep_model_constraints_deadline_and_retry_ownership() -> Result<()>
{
    let selected = |variable: &str| -> Result<PathBuf> {
        let path =
            std::env::var_os(variable).with_context(|| format!("required fixture: {variable}"))?;
        let path = PathBuf::from(path).canonicalize()?;
        ensure!(path.is_file(), "fixture is not a file: {}", path.display());
        Ok(path)
    };
    let user = selected("BITROUTER_TEST_SEMANTIC_GATEWAY_BINARY")?;
    let serving = selected("BITROUTER_TEST_INFERENCE_GATEWAY_BINARY")?;
    ensure!(
        user != serving,
        "heterogeneous roles require distinct binaries"
    );
    ensure!(
        capabilities(&user)?
            == json!({"semantic":true,"inference":false,"core":false,"harness":false}),
        "user gateway must be semantic-only"
    );
    ensure!(
        capabilities(&serving)?
            == json!({"semantic":false,"inference":true,"core":false,"harness":false}),
        "serving gateway must be inference-only"
    );
    topology(&user, &serving, true, true, true).await
}

#[tokio::test]
async fn two_gateway_processes_keep_model_constraints_deadline_and_retry_ownership() -> Result<()> {
    let binary = binary()?;
    let capabilities = capabilities(&binary)?;
    ensure!(capabilities["semantic"] == cfg!(feature = "semantic-routing"));
    ensure!(capabilities["inference"] == cfg!(feature = "inference-routing"));
    if !cfg!(feature = "core") {
        ensure!(capabilities["core"] == false);
    }
    if !cfg!(feature = "harness") {
        ensure!(capabilities["harness"] == false);
    }
    topology(
        &binary,
        &binary,
        cfg!(feature = "semantic-routing"),
        cfg!(feature = "inference-routing"),
        false,
    )
    .await
}

async fn topology(
    user_binary: &Path,
    serving_binary: &Path,
    semantic: bool,
    inference: bool,
    authenticated: bool,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let provider_address = address()?;
    let serving_address = address()?;
    let user_address = address()?;
    let journal_path = directory.path().join("provider.jsonl");
    let serving_path = directory.path().join("serving.json");
    let user_path = directory.path().join("user.json");
    let endpoint = format!("http://{provider_address}");
    let mut serving = json!({"inherit_defaults":false,"registry":{"enabled":false},"server":{"skip_auth":true},"database":{"url":format!("sqlite://{}?mode=rwc", directory.path().join("serving.db").display())},"providers":{}});
    for name in ["a", "b"] {
        serving["providers"][name] = json!({"api_base": endpoint,"api_key": format!("fixture-{name}"),"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"selected"}]});
    }
    serving["server"]["skip_auth"] = json!(!authenticated);
    if inference {
        serving["inference_routing"] = json!({"enabled":true,"limits":{"max_active":2,"max_uncertain":2},"queue":{"wait_timeout_ms":1000,"request_timeout_ms":5000},"destinations":[
            {"id":"a","provider":"a","model":"selected","endpoint":endpoint,"max_active":1},
            {"id":"b","provider":"b","model":"selected","endpoint":endpoint,"max_active":1}
        ]});
    }
    save(&serving_path, &serving)?;
    let selection = if semantic {
        json!({"kind":"policy","policy":"fixture","base_model":"base"})
    } else {
        json!({"kind":"model","model":"selected"})
    };
    let mut user = json!({"inherit_defaults":false,"registry":{"enabled":false},"server":{"skip_auth":true},"database":{"url":format!("sqlite://{}?mode=rwc", directory.path().join("user.db").display())},"providers":{"serving":{"api_base":format!("http://{serving_address}/v1"),"api_key":"gateway-only","api_protocol":[{"*":"chat_completions"}],"models":[{"id":"selected"}],"headers":{"x-bitrouter-timeout-ms":{"passthrough":true}}}},"routers":{"selected":{"selection":selection}}});
    user["server"]["skip_auth"] = json!(!authenticated);
    if semantic {
        let mut lock: bitrouter::policy_lock::PolicyLock = serde_saphyr::from_str(
            "lockfileVersion: 4\npolicies:\n  fixture:\n    tiers:\n      strong: {model: selected, context: evidence}\n    default_tier: strong\n    tool_use_tier: strong\n    tool_safe_tiers: [strong]\n",
        )?;
        lock.artifact = bitrouter::policy_lock::PolicyLock::default().artifact;
        let path = directory.path().join("policy-lock.yaml");
        std::fs::write(&path, bitrouter::policy_lock::deterministic_yaml(&lock)?)?;
        user["policy"] = json!({"path":path});
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut provider = spawn(
        serving_binary,
        "provider",
        &provider_address,
        &journal_path,
        &directory.path().join("provider"),
    )?;
    ready(&mut provider, &provider_address, &client).await?;
    let mut serving = spawn(
        serving_binary,
        "gateway",
        &serving_address,
        &serving_path,
        &directory.path().join("serving"),
    )?;
    ready(&mut serving, &serving_address, &client).await?;
    let serving_key = if authenticated {
        Some(std::fs::read_to_string(serving_path.with_extension("key"))?)
    } else {
        None
    };
    if let Some(key) = &serving_key {
        user["providers"]["serving"]["api_key"] = json!(key);
    }
    save(&user_path, &user)?;
    let mut user = spawn(
        user_binary,
        "gateway",
        &user_address,
        &user_path,
        &directory.path().join("user"),
    )?;
    ready(&mut user, &user_address, &client).await?;
    let url = format!("http://{user_address}/v1/chat/completions");
    let request = |text: &str, stream: bool| json!({"model":"bitrouter/selected","messages":[{"role":"user","content":text}],"stream":stream,"temperature":0.3,"max_tokens":32,"stop":["stop-here"]});
    let mut headers = reqwest::header::HeaderMap::new();
    let user_key = if authenticated {
        let key = std::fs::read_to_string(user_path.with_extension("key"))?;
        for address in [&user_address, &serving_address] {
            let response = client
                .post(format!("http://{address}/v1/chat/completions"))
                .json(&request("anonymous", false))
                .send()
                .await?;
            ensure!(response.status() == reqwest::StatusCode::UNAUTHORIZED);
        }
        let response = client
            .post(format!("http://{serving_address}/v1/chat/completions"))
            .bearer_auth(&key)
            .json(&request("wrong-hop", false))
            .send()
            .await?;
        ensure!(response.status() == reqwest::StatusCode::UNAUTHORIZED);
        ensure!(
            journal(&journal_path)?.is_empty(),
            "unauthenticated work reached provider"
        );
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {key}").parse()?,
        );
        Some(key)
    } else {
        None
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .default_headers(headers)
        .build()?;
    // Keep a real streaming request on A while the serving-side scheduler picks B.
    let held = client
        .post(&url)
        .header("x-bitrouter-timeout-ms", "4000")
        .json(&request("hold", true))
        .send()
        .await?;
    ensure!(
        held.status().is_success(),
        "held stream: {}",
        held.text().await?
    );
    let mut held = held.bytes_stream();
    ensure!(held.next().await.transpose()?.is_some());
    let result = client
        .post(&url)
        .header("x-bitrouter-timeout-ms", "2000")
        .json(&request("hello", false))
        .send()
        .await?;
    ensure!(result.status().is_success(), "{}", result.text().await?);
    drop(held);
    for streaming in [false, true] {
        let result = client
            .post(&url)
            .header("x-bitrouter-timeout-ms", "2000")
            .json(&request("hello", streaming))
            .send()
            .await?;
        ensure!(result.status().is_success());
        let body = result.text().await?;
        ensure!(body.contains("ready"));
        ensure!(
            !body.contains("generation") && !body.contains("affinity") && !body.contains("digest")
        );
        if streaming {
            ensure!(body.matches("[DONE]").count() == 1);
        }
    }
    let malformed = client
        .post(&url)
        .header("x-bitrouter-timeout-ms", "0")
        .json(&request("invalid", false))
        .send()
        .await?;
    ensure!(malformed.status().is_client_error());
    // One retry owner: the serving gateway makes at most A+B, the outer has one hop.
    let result = client
        .post(&url)
        .json(&request("reject", false))
        .send()
        .await?;
    ensure!(!result.status().is_success());
    let start = tokio::time::Instant::now();
    let result = client
        .post(&url)
        .header("x-bitrouter-timeout-ms", "200")
        .json(&request("slow", false))
        .send()
        .await?;
    ensure!(result.status().is_server_error());
    ensure!(start.elapsed() < Duration::from_secs(2));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if journal(&journal_path)?
                .iter()
                .any(|v| v["closed"] == "hold")
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let ingress = journal(&serving_path.with_extension("ingress.jsonl"))?;
    ensure!(
        ingress
            .iter()
            .filter_map(|event| event["timeout"].as_str()?.parse::<u64>().ok())
            .any(|ms| ms > 0 && ms < 200),
        "remaining deadline was not relayed: {ingress:?}"
    );
    let events = journal(&journal_path)?;
    let requests: Vec<_> = events.iter().filter(|v| v.get("body").is_some()).collect();
    let text = |value: &&Value| {
        value["body"]["messages"][0]["content"]
            .as_str()
            .map(str::to_owned)
    };
    ensure!(
        requests
            .iter()
            .filter(|v| text(v).as_deref() == Some("slow"))
            .count()
            == 1
    );
    ensure!(
        requests
            .iter()
            .filter(|v| text(v).as_deref() == Some("reject"))
            .count()
            <= 2
    );
    if inference {
        let held = requests
            .iter()
            .find(|v| text(v).as_deref() == Some("hold"))
            .context("held request")?;
        let next = requests
            .iter()
            .find(|v| text(v).as_deref() == Some("hello"))
            .context("adaptive request")?;
        ensure!(held["headers"]["authorization"] != next["headers"]["authorization"]);
    }
    ensure!(
        requests
            .iter()
            .all(|v| text(v).as_deref() != Some("invalid"))
    );
    ensure!(
        requests
            .iter()
            .any(|v| text(v).as_deref() == Some("reject"))
    );
    for request in requests {
        ensure!(request["body"]["model"] == "selected");
        ensure!(
            request["body"]["temperature"] == 0.3
                && request["body"]["max_tokens"] == 32
                && request["body"]["stop"] == json!(["stop-here"])
        );
        let serialized = serde_json::to_string(&request)?;
        for key in [&user_key, &serving_key].into_iter().flatten() {
            ensure!(
                !serialized.contains(key),
                "gateway credential reached provider"
            );
        }
        for internal in [
            "x-bitrouter-timeout-ms",
            "tenant_weights",
            "prefix_affinity",
            "namespace",
            "generation",
            "gateway-only",
        ] {
            ensure!(!serialized.contains(internal), "leaked {internal}");
        }
    }
    Ok(())
}
