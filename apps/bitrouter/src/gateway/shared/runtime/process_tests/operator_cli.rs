//! Invoke the stock remote CLI against independently running gateway replicas.

use super::*;
use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

const TOKEN: &str = "fixture-operator-cli-process-0123456789abcdef";

async fn command(
    fixture: &mut Fixture,
    args: &[String],
    token: Option<OsString>,
) -> Result<(ExitStatus, String, String)> {
    let index = fixture.children.len();
    let stdout = fixture.root.path().join(format!("gateway-{index}.json"));
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let mut command = Command::new(&fixture.binary);
    command
        .args(args)
        .current_dir(fixture.root.path().join("launch"))
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env("FIXTURE_SHARED_DATABASE", "must-not-open")
        .env("FIXTURE_SHARED_KEY", "must-not-use")
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .env_remove("FIXTURE_OPERATOR_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(&stdout)?))
        .stderr(Stdio::from(std::fs::File::create(&log)?))
        .kill_on_drop(true);
    if let Some(token) = token {
        command.env("FIXTURE_OPERATOR_TOKEN", token);
    }
    let child = command.spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address: ([127, 0, 0, 1], 0).into(),
    });
    let status = fixture.child(index)?.exited().await?;
    Ok((
        status,
        tokio::fs::read_to_string(stdout).await?,
        fixture.child(index)?.logs(),
    ))
}

fn args(endpoint: &str, action: &str, selectors: &[&str]) -> Vec<String> {
    [
        "shared",
        action,
        "--endpoint",
        endpoint,
        "--token-env",
        "FIXTURE_OPERATOR_TOKEN",
    ]
    .into_iter()
    .chain(selectors.iter().copied())
    .map(str::to_owned)
    .collect()
}

fn one_json(output: &str) -> Result<Value> {
    let mut values = serde_json::Deserializer::from_str(output).into_iter::<Value>();
    let value = values.next().context("missing CLI JSON output")??;
    ensure!(
        values.next().is_none(),
        "CLI emitted more than one JSON value"
    );
    Ok(value)
}

#[tokio::test]
#[ignore = "requires PostgreSQL and BITROUTER_TEST_GATEWAY_BINARY"]
async fn stock_operator_cli_recovers_across_replicas_without_local_state() -> Result<()> {
    let mut fixture = Fixture::new(true).await?;
    let result = async {
        let a_address = available_address()?;
        let a = fixture.spawn_with_operator("serve", available_address()?, &[], Some((a_address,TOKEN))).await?;
        fixture.child(a)?.ready().await?;
        let b_address = available_address()?;
        let b = fixture.spawn_with_operator("serve", available_address()?, &[], Some((b_address,TOKEN))).await?;
        fixture.child(b)?.ready().await?;
        let a_endpoint = format!("http://{a_address}");
        let b_endpoint = format!("http://{b_address}");
        let path = fixture.root.path().join("gateway-cli-input.json");
        let path_text = path.to_str().context("fixture input path is not UTF-8")?;
        let id = "pool/東京%&+?";
        tokio::fs::write(&path, serde_json::to_vec(&json!({"operation_id":"..","command":{"action":"put_resource","input":{
            "resource_pool_id":id,"expected_revision":null,"concurrent_attempts":4
        }}}))?).await?;
        let (status, stdout, stderr) = command(&mut fixture, &args(&a_endpoint,"apply", &["--input",path_text]), Some(TOKEN.into())).await?;
        ensure!(status.success(), "stock apply failed: {stdout} {stderr}");
        let receipt = one_json(&stdout)?;
        ensure!(receipt["operation_id"] == ".." && receipt["operator_id"] == "fixture-operator");
        ensure!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        let (status, stdout, _) = command(&mut fixture, &args(&b_endpoint,"operation", &["--operation-id",".."]), Some(TOKEN.into())).await?;
        ensure!(status.success());
        ensure!(one_json(&stdout)? == json!({"status":"committed","receipt":receipt}));
        let (status, stdout, _) = command(&mut fixture, &args(&b_endpoint,"resource", &["--id",id]), Some(TOKEN.into())).await?;
        ensure!(status.success() && one_json(&stdout)?["concurrent_attempts"] == 4);
        for human in [false, true] {
            let mut call = args(&b_endpoint,"operation", &["--operation-id","unresolved"]);
            if human { call.insert(0,"--human".into()); }
            let (status, stdout, stderr) = command(&mut fixture, &call, Some(TOKEN.into())).await?;
            ensure!(status.success());
            if human { ensure!(stdout.contains("does not prove rollback")); }
            else { ensure!(one_json(&stdout)? == json!({"status":"unresolved","operation_id":"unresolved"})); }
            ensure!(!stderr.contains(TOKEN));
        }
        tokio::fs::write(&path, br#"{"operation_id":"..","command":{"action":"import_credential","input":{"credential":"PRIVATE_INPUT_SENTINEL"}}}"#).await?;
        for human in [false, true] {
            let mut call = args(&a_endpoint,"apply", &["--input",path_text]);
            if human { call.insert(0,"--human".into()); }
            let (status, stdout, stderr) = command(&mut fixture, &call, Some(TOKEN.into())).await?;
            ensure!(!status.success() && stdout.contains("invalid operator mutation JSON"));
            if !human { one_json(&stdout)?; }
            ensure!(!stdout.contains("PRIVATE_INPUT_SENTINEL") && !stderr.contains("PRIVATE_INPUT_SENTINEL"));
        }
        for human in [false, true] {
            let mut call = args(&a_endpoint,"apply", &["--input","-"]);
            let position = call.iter().position(|arg| arg == "--token-env").context("token flag missing")?;
            call[position+1] = "PRIVATE_ARGUMENT_SENTINEL+TOKEN".into();
            if human { call.insert(0,"--human".into()); }
            let (status, stdout, stderr) = command(&mut fixture, &call, None).await?;
            ensure!(!status.success() && stdout.contains("invalid operator token environment reference"));
            if !human { one_json(&stdout)?; }
            ensure!(!stdout.contains("PRIVATE_ARGUMENT_SENTINEL") && !stderr.contains("PRIVATE_ARGUMENT_SENTINEL"));
        }
        let mut invalid_token = b"PRIVATE_TOKEN_SENTINEL".to_vec();
        invalid_token.push(255);
        for token in [None, Some(OsString::from_vec(invalid_token))] {
            let (status, stdout, stderr) = command(&mut fixture, &args(&a_endpoint,"apply", &["--input","-"]), token).await?;
            ensure!(!status.success() && stdout.contains("operator token environment variable is unavailable"));
            one_json(&stdout)?;
            ensure!(!stdout.contains("PRIVATE_TOKEN_SENTINEL") && !stderr.contains("PRIVATE_TOKEN_SENTINEL"));
        }
        for index in [a,b] { ensure!(fixture.child(index)?.stop().await?.success()); }
        ensure!(fixture.owners().await?.len() == 2, "remote CLI created a serving incarnation");
        fixture.no_local_files()?;
        Ok(())
    }.await;
    fixture.finish(result).await
}
