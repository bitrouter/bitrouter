use super::*;

pub(super) async fn barrier(profile: &Profile, stage: &str) -> Result<()> {
    let Ok(path) = std::env::var("BITROUTER_AGENT_TEST_BARRIER") else {
        return Ok(());
    };
    if std::env::var("BITROUTER_AGENT_TEST_BARRIER_STAGE").as_deref() != Ok(stage) {
        return Ok(());
    }
    if !path.starts_with("/tmp/bitrouter-agent-oss-")
        || std::env::var("BITROUTER_AGENT_OWNED_FIXTURE").as_deref() != Ok("owned-loopback-only")
    {
        bail!("owned process barrier required");
    }
    let status = pending_status(profile)?;
    std::fs::write(&path, serde_json::to_vec(&status)?)?;
    for _ in 0..120 {
        if std::path::Path::new(&(path.clone() + ".release")).exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    bail!("owned process barrier expired")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessInput {
    root: PathBuf,
    issuer: String,
    resource: String,
    profile: String,
    name: String,
    operation: String,
}

#[tokio::test]
#[ignore = "explicit owned native Console process fixture required"]
async fn native_console_client_process() -> Result<()> {
    let path =
        std::env::var("BITROUTER_AGENT_PROCESS_INPUT").context("owned process input required")?;
    if !path.starts_with("/tmp/bitrouter-agent-oss-")
        || std::env::var("BITROUTER_AGENT_OWNED_FIXTURE").as_deref() != Ok("owned-loopback-only")
    {
        bail!("owned process input required");
    }
    let input: ProcessInput = serde_json::from_slice(&std::fs::read(&path)?)?;
    let issuer = reqwest::Url::parse(&input.issuer)?;
    if issuer.scheme() != "http"
        || issuer.host_str() != Some("127.0.0.1")
        || issuer.port().is_none()
        || !input
            .root
            .to_string_lossy()
            .starts_with("/tmp/bitrouter-agent-oss-")
    {
        bail!("owned loopback Console required");
    }
    let controller = Controller {
        root: input.root,
        authority: None,
    };
    let status = match input.operation.as_str() {
        "begin" | "renew" => {
            controller
                .begin(
                    &input.profile,
                    &input.name,
                    &input.issuer,
                    &input.resource,
                    KeyStorage::File,
                    input.operation == "renew",
                )
                .await?
        }
        "status" => controller.status(&input.profile).await?,
        "disconnect" => controller.disconnect(&input.profile).await?,
        "expire_local_access" => {
            let store = Store::lock(&controller.root, &input.profile)?;
            let mut profile = store.load()?.context("owned profile missing")?;
            profile
                .current
                .as_mut()
                .context("owned current session missing")?
                .expires_at = now() - 1;
            store.save(&profile)?;
            pending_status(&profile)?
        }
        _ => bail!("unknown owned process operation"),
    };
    std::fs::write(
        std::path::Path::new(&path).with_extension("receipt.json"),
        serde_json::to_vec(&status)?,
    )?;
    Ok(())
}

#[test]
#[ignore = "explicit owned new OS key store entry only"]
fn owned_keychain_roundtrip() -> Result<()> {
    if std::env::var("BITROUTER_AGENT_KEYCHAIN_TEST").as_deref() != Ok("owned-new-entry-only") {
        bail!("owned key store acknowledgement required");
    }
    let key = store::create_key(KeyStorage::Keychain)?;
    let result = (|| {
        let first = crypto::public_jwk(&store::read_key(&key)?)?;
        let second = crypto::public_jwk(&store::read_key(&key)?)?;
        assert_eq!(first, second);
        Ok(())
    })();
    store::remove_key(&key)?;
    result
}

#[test]
fn key_proofs_bind_token_method_url_and_have_fresh_jti() -> Result<()> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
    let secret = crypto::generate()?;
    let jwk = crypto::public_jwk(&secret)?;
    let mut bytes = vec![4];
    bytes.extend(B64.decode(text(&jwk, "x", 43)?)?);
    bytes.extend(B64.decode(text(&jwk, "y", 43)?)?);
    let proof = crypto::proof(
        &secret,
        "https://api.example/v1/status",
        "GET",
        Some("nonce"),
        Some("secret-token"),
    )?;
    let parts: Vec<&str> = proof.split('.').collect();
    assert_eq!(parts.len(), 3);
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &bytes)
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &B64.decode(parts[2])?,
        )
        .map_err(|_| anyhow::anyhow!("proof signature did not verify"))?;
    let claims: Value = serde_json::from_slice(&B64.decode(parts[1])?)?;
    assert_eq!(claims["htu"], "https://api.example/v1/status");
    assert_eq!(claims["nonce"], "nonce");
    assert!(claims["ath"].is_string());
    assert!(!proof.contains("secret-token"));
    assert!(!jwk.to_string().contains("pkcs8"));
    let another = crypto::proof(
        &secret,
        "https://api.example/v1/status",
        "GET",
        Some("nonce"),
        Some("secret-token"),
    )?;
    assert_ne!(proof, another);
    Ok(())
}
#[test]
fn profile_writes_are_private_durable_and_locked_across_handles() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("profiles");
    let store = Store::lock(&root, "fixture")?;
    let profile = Profile {
        format: 1,
        id: "fixture".into(),
        issuer: "https://console.example".into(),
        resource: "https://api.example".into(),
        key: Some(store::create_key(KeyStorage::File)?),
        current: None,
        pending: None,
        refresh_uncertain: false,
        disconnecting: false,
        disconnected: false,
    };
    store.save(&profile)?;
    assert_eq!(
        store.load()?.context("saved profile missing")?.id,
        "fixture"
    );
    assert!(Store::lock(&root, "fixture").is_err());
    assert!(Store::lock(&root, "../escape").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&store.path)?.permissions().mode() & 0o777,
            0o600
        );
    }
    Ok(())
}
#[test]
fn unsafe_origins_and_bearer_tokens_never_get_agent_fallback() {
    for issuer in [
        "http://console.example",
        "https://u:p@console.example",
        "https://console.example/path",
        "https://console.example?q=x",
    ] {
        assert!(wire::origin(issuer).is_err());
    }
    assert!(
        tokens(
            json!({"token_type":"Bearer","access_token":"secret"}),
            "client"
        )
        .is_err()
    );
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_advertises_only_local_auth_tools_and_rejects_a_model_supplied_issuer() -> Result<()> {
    use rmcp::{ServiceExt, model::CallToolRequestParams};
    let directory = tempfile::tempdir()?;
    let bridge = super::mcp::fixture(directory.path().join("profiles"));
    let (server_io, client_io) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        bridge.serve(server_io).await?.waiting().await?;
        Ok::<(), anyhow::Error>(())
    });
    let client = ().serve(client_io).await?;
    let mut names: Vec<String> = client
        .list_all_tools()
        .await?
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["agent_connect", "agent_disconnect", "agent_status"]
    );
    let result = client
        .call_tool(CallToolRequestParams::new("agent_status").with_arguments(
            serde_json::from_value(json!({"profile":"example","issuer":"https://other.example"}))?,
        ))
        .await;
    assert!(result.is_err() || result.is_ok_and(|result| result.is_error == Some(true)));
    assert!(!directory.path().join("profiles").exists());
    for (id, issuer, resource) in [
        (
            "other_issuer",
            "https://other.invalid",
            "https://api.example",
        ),
        (
            "other_resource",
            "https://console.example",
            "https://other.invalid",
        ),
    ] {
        {
            let store = Store::lock(&directory.path().join("profiles"), id)?;
            store.save(&Profile {
                format: 1,
                id: id.into(),
                issuer: issuer.into(),
                resource: resource.into(),
                key: None,
                current: None,
                pending: None,
                refresh_uncertain: false,
                disconnecting: false,
                disconnected: false,
            })?;
        }
        for tool in ["agent_status", "agent_disconnect"] {
            let result = client
                .call_tool(
                    CallToolRequestParams::new(tool)
                        .with_arguments(serde_json::from_value(json!({"profile":id}))?),
                )
                .await;
            let error = match result {
                Err(error) => error.to_string(),
                Ok(result) => serde_json::to_string(&result)?,
            };
            assert!(error.contains("saved Agent authority differs from this MCP bridge"));
            let store = Store::lock(&directory.path().join("profiles"), id)?;
            let unchanged = store.load()?.context("owned profile missing")?;
            assert!(!unchanged.disconnecting && !unchanged.disconnected);
        }
    }
    client.cancel().await?;
    task.await??;
    Ok(())
}
