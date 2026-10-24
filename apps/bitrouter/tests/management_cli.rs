//! CLI-only control journeys over the actual daemon and its existing services.
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "support/management.rs"]
mod fixture;
use fixture::{Fixture, path as text_path};

#[path = "management_cli/skill_delivery.rs"]
mod skill_delivery;

#[path = "management_cli/evolution.rs"]
mod evolution;

#[tokio::test]
async fn management_protocol_versions_reject_new_operations_before_effects() -> Result<()> {
    use bitrouter::daemon::{DaemonCommand, DaemonResponse};
    use bitrouter::supervisor::{
        ControlFence, SessionCommand, SessionRequest, SessionResponse, SessionScope,
    };
    let fixture = Fixture::new(None, None).await?;
    let socket = bitrouter::agent_local::socket_path(&fixture.socket);
    let stream = tokio::net::UnixStream::connect(&socket).await?;
    let mut stream = BufReader::new(stream);
    let mut old_create = serde_json::to_vec(
        &json!({"version":18,"command_id":"old-create","server_instance_id":null,"op":"create_thread","workspace":fixture.workspace,"model":"model","effort":null,"verification_command":null,"idempotency_key":"never-accepted"}),
    )?;
    old_create.push(b'\n');
    stream.get_mut().write_all(&old_create).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line)).await??;
    let rejected: Value = serde_json::from_str(&line)?;
    assert_eq!(rejected["type"], "unsupported_version");
    assert_eq!(rejected["supported"], 19);
    let capabilities =
        bitrouter::agent_local::request(&socket, bitrouter::agent_local::Operation::Capabilities)
            .await?;
    let bitrouter::agent_local::ReplyResult::Capabilities { operations, .. } = capabilities else {
        anyhow::bail!("missing native capabilities")
    };
    for operation in ["load_thread", "close_thread", "recover_thread"] {
        ensure!(operations.iter().any(|name| name == operation));
    }
    assert!(
        fixture.run(&["threads", "list"]).await?["payload"]["page"]["entries"]
            .as_array()
            .context("native directory")?
            .is_empty()
    );

    let mut authorize = SessionRequest::authorize(
        "legacy-reader".into(),
        [SessionScope::List, SessionScope::Peek].into(),
    );
    authorize.version = 1;
    let granted = bitrouter::daemon::send_command(
        &fixture.socket,
        &DaemonCommand::Sessions { request: authorize },
    )
    .await?;
    let DaemonResponse::Sessions {
        response: SessionResponse::Authorized { grant },
    } = granted
    else {
        anyhow::bail!("v1 grant issuance failed")
    };
    let mut request = SessionRequest::authorized(&grant, SessionCommand::List);
    request.version = 1;
    ensure!(matches!(
        bitrouter::daemon::send_command(
            &fixture.socket,
            &DaemonCommand::Sessions {
                request: request.clone()
            }
        )
        .await?,
        DaemonResponse::Sessions {
            response: SessionResponse::RunSummaries { .. }
        }
    ));
    request.command = SessionCommand::ActionStatus {
        fence: ControlFence {
            run_id: "unknown".into(),
            client_id: "legacy-reader".into(),
            lease_generation: 0,
            action_request_id: "unknown".into(),
        },
    };
    let DaemonResponse::Error { message } = bitrouter::daemon::send_command(
        &fixture.socket,
        &DaemonCommand::Sessions {
            request: request.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("v1 accepted v2 action status")
    };
    ensure!(message.contains("requires session protocol version 2"));
    request.version = 99;
    let DaemonResponse::Error { message } =
        bitrouter::daemon::send_command(&fixture.socket, &DaemonCommand::Sessions { request })
            .await?
    else {
        anyhow::bail!("future session protocol was accepted")
    };
    ensure!(message.contains("unsupported session protocol version"));
    Ok(())
}

#[tokio::test]
async fn headless_foreground_owner_retains_worktree_claim_against_cli_background_start()
-> Result<()> {
    use bitrouter::supervisor::{PermissionPolicy, Presentation, StartRunRequest};
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let git = tokio::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(&fixture.workspace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .await?;
    ensure!(git.status.success(), "fixture git init failed");
    let nested = fixture.workspace.join("nested");
    std::fs::create_dir(&nested)?;
    let owner = bitrouter::agent_sessions::BackgroundClient::new(fixture.socket.clone());
    let foreground = owner
        .start(StartRunRequest {
            action_request_id: "headless-foreground".into(),
            client_id: None,
            label: None,
            agent_id: "stub".into(),
            prompt: None,
            cwd: fixture.workspace.clone(),
            routing: bitrouter::acp_cli::RoutingOptions {
                direct: true,
                ..Default::default()
            },
            launch: Default::default(),
            session: Default::default(),
            presentation: Presentation::Foreground,
            parent_run_id: None,
            allow_shared_directory: false,
            permission_policy: PermissionPolicy::default(),
            result_schema: None,
        })
        .await?;
    let run = foreground.snapshot.run_id;
    idle(&fixture, &run).await?;
    let rejected = fixture
        .rejected(&[
            "run",
            "stub",
            "must not share foreground worktree",
            "--direct",
            "--background",
            "--cwd",
            text_path(&nested)?,
        ])
        .await?;
    let text = rejected.to_string();
    ensure!(
        text.contains("worktree") || text.contains("claimed"),
        "claim conflict missing: {text}"
    );
    let retained = fixture.run(&["agents", "show", &run]).await?;
    assert_eq!(retained["payload"]["presentation"], "foreground");
    assert_eq!(retained["payload"]["process"], "running");
    owner.stop(&run).await?;
    owner.release_all().await;
    Ok(())
}

fn field<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {pointer}: {value}"))
}

async fn idle(fixture: &Fixture, run: &str) -> Result<Value> {
    fixture
        .wait(&["agents", "show", run], |value| {
            value["payload"]["turn"] == "idle"
        })
        .await
}

#[tokio::test]
async fn acp_cli_permissions_observation_cancel_and_retained_lifecycle() -> Result<()> {
    let fixture = Fixture::new(Some("overlapping-permissions"), None).await?;
    let run = fixture
        .background("ask through a separate CLI", &["--direct"])
        .await?;
    let pending = fixture
        .wait(&["agents", "pending", &run], |value| {
            value["payload"]["pending"]
                .as_array()
                .is_some_and(|rows| rows.len() == 2)
        })
        .await?;
    let rows = pending["payload"]["pending"]
        .as_array()
        .context("pending permissions")?;
    let first = field(&rows[0], "/permission_id")?.to_owned();
    let second = field(&rows[1], "/permission_id")?.to_owned();
    fixture
        .rejected(&[
            "agents",
            "answer",
            &run,
            "--request",
            "wrong-request",
            "--option",
            "allow-1",
        ])
        .await?;
    fixture
        .rejected(&[
            "agents",
            "answer",
            &run,
            "--request",
            &first,
            "--option",
            "not-offered",
        ])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "pending", &run]).await?["payload"]["pending"]
            .as_array()
            .context("pending")?
            .len(),
        2
    );
    // Watching never acquires control and disconnect never cancels accepted work.
    let mut observer = fixture
        .command()
        .args(["agents", "events", &run, "--follow"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut stdout = BufReader::new(observer.stdout.take().context("observer stdout")?);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line)).await??;
    let event: Value = serde_json::from_str(&line)?;
    assert_eq!(event["kind"], "acp_events");
    observer.start_kill()?;
    observer.wait().await?;
    assert_eq!(
        fixture.run(&["agents", "pending", &run]).await?["payload"]["pending"]
            .as_array()
            .context("pending")?
            .len(),
        2
    );
    for (permission, option_kind) in [(&first, "allow_once"), (&second, "reject_once")] {
        let row = rows
            .iter()
            .find(|row| row["permission_id"] == *permission)
            .context("permission")?;
        let option = row["options"]
            .as_array()
            .context("options")?
            .iter()
            .find(|option| option["kind"] == option_kind)
            .context("offered option")?;
        fixture
            .run(&[
                "agents",
                "answer",
                &run,
                "--request",
                permission,
                "--option",
                field(option, "/optionId")?,
            ])
            .await?;
    }
    let completed = idle(&fixture, &run).await?;
    assert_eq!(completed["payload"]["review"], "unread");
    assert!(completed["payload"]["lease"].is_null());
    fixture
        .rejected(&[
            "agents",
            "answer",
            &run,
            "--request",
            &first,
            "--option",
            "allow-1",
        ])
        .await?;
    fixture.run(&["agents", "review", &run]).await?;
    assert_eq!(idle(&fixture, &run).await?["payload"]["review"], "reviewed");
    let prompt = fixture.prompt("again.txt", "another explicit prompt")?;
    fixture
        .run(&[
            "agents",
            "prompt",
            &run,
            "--prompt-file",
            text_path(&prompt)?,
        ])
        .await?;
    let fresh = fixture
        .wait(&["agents", "pending", &run], |value| {
            value["payload"]["pending"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        })
        .await?;
    ensure!(
        fresh["payload"]["pending"]
            .as_array()
            .context("pending")?
            .iter()
            .all(|row| row["permission_id"] != first && row["permission_id"] != second),
        "old permission was rebound to a new turn"
    );
    fixture.run(&["agents", "cancel", &run]).await?;
    idle(&fixture, &run).await?;
    assert_eq!(
        fixture.run(&["agents", "pending", &run]).await?["payload"]["pending"],
        json!([])
    );
    let events = fixture.run(&["agents", "events", &run]).await?;
    ensure!(
        !events["payload"]["replay"]["events"]
            .as_array()
            .context("events")?
            .is_empty()
    );
    let human = fixture.output(&["agents", "show", &run, "--human"]).await?;
    ensure!(human.status.success() && !human.stdout.contains(&0x1b));
    fixture.run(&["agents", "stop", &run]).await?;
    assert_eq!(fixture.run(&["agents"]).await?["instances"], json!([]));
    ensure!(
        fixture.run(&["agents", "sessions"]).await?["runs"]
            .as_array()
            .context("history")?
            .iter()
            .any(|row| row["run_id"] == run)
    );
    fixture.run(&["agents", "remove", &run]).await?;
    assert!(
        fixture.run(&["agents", "sessions"]).await?["runs"]
            .as_array()
            .context("history")?
            .is_empty()
    );
    // ABI/native protocol IDs remain separate namespaces.
    fixture.rejected(&["threads", "show", &run]).await?;
    Ok(())
}

#[tokio::test]
async fn lost_acp_ack_retries_the_exact_fence_and_can_be_inspected_without_replay() -> Result<()> {
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let run = fixture.background("initial", &["--direct"]).await?;
    idle(&fixture, &run).await?;
    let socket = fixture.directory.path().join("proxy.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let mutations = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture = mutations.clone();
    let actual = fixture.socket.clone();
    let proxy: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let command: bitrouter::daemon::DaemonCommand = serde_json::from_str(&line)?;
            let is_mutation = matches!(&command, bitrouter::daemon::DaemonCommand::Sessions { request } if matches!(&request.command, bitrouter::supervisor::SessionCommand::Mutate(_)));
            let response = bitrouter::daemon::send_command(&actual, &command).await?;
            let drop_ack = if is_mutation {
                let mut captured = capture
                    .lock()
                    .map_err(|_| anyhow::anyhow!("fixture lock poisoned"))?;
                captured.push(serde_json::from_str(&line)?);
                captured.len() == 1
            } else {
                false
            };
            if !drop_ack {
                let mut encoded = serde_json::to_vec(&response)?;
                encoded.push(b'\n');
                reader.get_mut().write_all(&encoded).await?;
            }
        }
    });
    let prompt = fixture.prompt("retry.txt", "one command despite lost ACK")?;
    let accepted = fixture
        .run(&[
            "agents",
            "prompt",
            &run,
            "--socket",
            text_path(&socket)?,
            "--prompt-file",
            text_path(&prompt)?,
            "--request-id",
            "original-action",
        ])
        .await?;
    proxy.abort();
    let _ = proxy.await;
    {
        let captured = mutations
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture lock poisoned"))?;
        assert_eq!(captured.len(), 2);
        ensure!(
            captured[0] == captured[1],
            "ACK retry changed the original request or authorization"
        );
    }
    idle(&fixture, &run).await?;
    let client = field(&accepted, "/payload/client_id")?;
    let generation = accepted["payload"]["lease_generation"]
        .as_u64()
        .context("generation")?
        .to_string();
    let instance = field(&accepted, "/server_instance_id")?;
    let status = fixture
        .run(&[
            "agents",
            "action",
            &run,
            "--request-id",
            "original-action",
            "--client-id",
            client,
            "--lease-generation",
            &generation,
            "--server-instance",
            instance,
        ])
        .await?;
    assert_eq!(
        status["payload"]["acknowledgement"]["action_request_id"],
        "original-action"
    );
    fixture
        .run(&[
            "agents",
            "prompt",
            &run,
            "--prompt-file",
            text_path(&prompt)?,
            "--request-id",
            "original-action",
            "--client-id",
            client,
            "--lease-generation",
            &generation,
            "--server-instance",
            instance,
        ])
        .await?;
    assert_eq!(
        fixture
            .captured()?
            .iter()
            .filter(|request| request["method"] == "session/prompt")
            .count(),
        2
    );
    fixture
        .rejected(&[
            "agents",
            "action",
            &run,
            "--request-id",
            "original-action",
            "--client-id",
            "foreign-client",
            "--lease-generation",
            &generation,
            "--server-instance",
            instance,
        ])
        .await?;
    fixture
        .rejected(&["agents", "review", &run, "--server-instance", "old-boot"])
        .await?;
    fixture.run(&["agents", "review", &run]).await?;
    fixture
        .rejected(&[
            "agents",
            "action",
            &run,
            "--request-id",
            "original-action",
            "--client-id",
            client,
            "--lease-generation",
            &generation,
            "--server-instance",
            instance,
        ])
        .await?;
    Ok(())
}

#[tokio::test]
async fn acp_cli_rejects_control_boot_change_during_authorization_before_mutation() -> Result<()> {
    use bitrouter::daemon::{DaemonCommand, DaemonResponse};
    use bitrouter::supervisor::SessionCommand;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let run = fixture
        .background("unreviewed result", &["--direct"])
        .await?;
    assert_eq!(idle(&fixture, &run).await?["payload"]["review"], "unread");
    let socket = fixture.directory.path().join("boot-proxy.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let actual = fixture.socket.clone();
    let mutations = Arc::new(AtomicUsize::new(0));
    let observed = mutations.clone();
    let mut proxy = tokio::task::JoinSet::<Result<()>>::new();
    proxy.spawn(async move {
        let mut authorized = false;
        loop {
            let (stream, _) = listener.accept().await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let command: DaemonCommand = serde_json::from_str(&line)?;
            if let DaemonCommand::Sessions { request } = &command {
                match &request.command {
                    SessionCommand::Authorize { .. } => authorized = true,
                    SessionCommand::Mutate(_) => {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            let mut response = bitrouter::daemon::send_command(&actual, &command).await?;
            // The first hello predates grant issuance. Authorization and every
            // later request are served by the replacement boot.
            if !authorized && let DaemonResponse::HandoffHello { instance_id, .. } = &mut response {
                *instance_id = "previous-control-boot".into();
            }
            let mut encoded = serde_json::to_vec(&response)?;
            encoded.push(b'\n');
            reader.get_mut().write_all(&encoded).await?;
        }
    });
    let rejected = fixture
        .rejected(&[
            "agents",
            "review",
            &run,
            "--socket",
            text_path(&socket)?,
            "--server-instance",
            "previous-control-boot",
        ])
        .await?;
    ensure!(
        rejected.to_string().contains("changed"),
        "boot change was not explained: {rejected}"
    );
    assert_eq!(mutations.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.run(&["agents", "show", &run]).await?["payload"]["review"],
        "unread"
    );
    Ok(())
}

#[tokio::test]
async fn evolution_cli_checks_control_boot_at_the_mutation_owner() -> Result<()> {
    use bitrouter::daemon::DaemonCommand;
    use std::sync::atomic::{AtomicU16, Ordering};
    let fixture = Fixture::new(None, None).await?;
    let socket = fixture.directory.path().join("evolution-proxy.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let actual = fixture.socket.clone();
    let advertised = Arc::new(AtomicU16::new(1));
    let wire_version = advertised.clone();
    let mut proxy = tokio::task::JoinSet::<Result<()>>::new();
    proxy.spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let command: DaemonCommand = serde_json::from_str(&line)?;
            let discovery = serde_json::from_str::<Value>(&line)?["cmd"]
                .as_str()
                .is_some_and(|command| matches!(command, "handoff_hello" | "evolution_hello"));
            let response = bitrouter::daemon::send_command(&actual, &command).await?;
            let mut reply = serde_json::to_value(response)?;
            if discovery {
                reply["instance_id"] = json!("previous-control-boot");
                reply["server_instance_id"] = json!("previous-control-boot");
                let version = wire_version.load(Ordering::SeqCst);
                if version == 0 {
                    reply = serde_json::to_value(bitrouter::daemon::DaemonResponse::Error {
                        message: "legacy daemon has no evolution hello".into(),
                    })?;
                } else {
                    reply["version"] = json!(version);
                }
            }
            let mut encoded = serde_json::to_vec(&reply)?;
            encoded.push(b'\n');
            reader.get_mut().write_all(&encoded).await?;
        }
    });
    let rejected = fixture
        .rejected(&[
            "acp",
            "evolution",
            "--socket",
            text_path(&socket)?,
            "--server-instance",
            "previous-control-boot",
            "mode",
            "manual",
        ])
        .await?;
    ensure!(
        rejected.to_string().contains("instance"),
        "owner fence was not explained: {rejected}"
    );
    assert_eq!(
        fixture.run(&["acp", "evolution", "status"]).await?["report"]["control"]["mode"],
        "off"
    );
    for version in [0, 99] {
        advertised.store(version, Ordering::SeqCst);
        let rejected = fixture
            .rejected(&[
                "acp",
                "evolution",
                "--socket",
                text_path(&socket)?,
                "mode",
                "manual",
            ])
            .await?;
        ensure!(
            rejected.to_string().contains("evolution control"),
            "unsupported contract was not diagnosed: {rejected}"
        );
        assert_eq!(
            fixture.run(&["acp", "evolution", "status"]).await?["report"]["control"]["mode"],
            "off"
        );
    }
    Ok(())
}

#[tokio::test]
async fn acp_cli_settings_sessions_routes_and_controller_lease_are_scoped() -> Result<()> {
    let upstream = MockServer::start().await;
    let fixture = Fixture::new(Some("control-settings"), Some(&upstream.uri())).await?;
    let run = fixture.background("settings", &["--direct"]).await?;
    let first = idle(&fixture, &run).await?;
    let first_session = field(&first, "/payload/native_session_id")?.to_owned();
    assert_eq!(
        fixture.run(&["agents", "mode", "show", &run]).await?["payload"]["modes"]["currentModeId"],
        "work"
    );
    fixture
        .run(&["agents", "mode", "set", &run, "plan"])
        .await?;
    fixture
        .rejected(&["agents", "mode", "set", &run, "rejected"])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "mode", "show", &run]).await?["payload"]["modes"]["currentModeId"],
        "plan"
    );
    fixture
        .run(&[
            "agents",
            "config",
            "set",
            &run,
            "a12-setting",
            "--value-json",
            "\"a12-confirmed\"",
        ])
        .await?;
    fixture
        .rejected(&[
            "agents",
            "config",
            "set",
            &run,
            "a12-setting",
            "--value-json",
            "\"rejected\"",
        ])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "config", "show", &run]).await?["payload"]["options"][0]["currentValue"],
        "a12-confirmed"
    );
    fixture.rejected(&["agents", "route", "show", &run]).await?;
    let directory = fixture
        .run(&[
            "agents",
            "session",
            "list",
            &run,
            "--cwd",
            text_path(&fixture.workspace)?,
        ])
        .await?;
    assert_eq!(
        directory["payload"]["response"]["sessions"][0]["sessionId"],
        "control-saved"
    );
    fixture.run(&["agents", "session", "new", &run]).await?;
    let fresh = idle(&fixture, &run).await?;
    assert_ne!(fresh["payload"]["native_session_id"], first_session);
    assert_eq!(
        fresh["payload"]["initial_settings"]["modes"]["currentModeId"],
        "work"
    );
    fixture
        .run(&["agents", "session", "load", &run, "control-saved"])
        .await?;
    assert_eq!(
        idle(&fixture, &run).await?["payload"]["native_session_id"],
        "control-saved"
    );
    fixture
        .run(&["agents", "session", "resume", &run, &first_session])
        .await?;
    assert_eq!(
        idle(&fixture, &run).await?["payload"]["native_session_id"],
        first_session
    );
    fixture.control("settings-update").await?;
    fixture
        .wait(&["agents", "config", "show", &run], |value| {
            value["payload"]["options"][0]["currentValue"] == "a12-confirmed"
        })
        .await?;
    assert_eq!(
        fixture.run(&["agents", "mode", "show", &run]).await?["payload"]["modes"]["currentModeId"],
        "plan"
    );
    // A read has no control lease and cannot displace an existing controller.
    let controller = bitrouter::agent_sessions::BackgroundClient::new(fixture.socket.clone());
    let attachment = controller.attach(&run, false).await?;
    let held = attachment
        .snapshot
        .lease
        .context("attached control lease")?;
    let observed = fixture.run(&["agents", "show", &run]).await?;
    assert_eq!(
        observed["payload"]["lease"]["owner_client_id"],
        held.owner_client_id
    );
    fixture
        .rejected(&["agents", "mode", "set", &run, "work"])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "show", &run]).await?["payload"]["lease"]["generation"],
        held.generation
    );
    controller.release_all().await;
    fixture
        .run(&["agents", "mode", "set", &run, "work"])
        .await?;
    fixture.run(&["agents", "stop", &run]).await?;

    let routed = fixture
        .background("routed settings", &["--model", "mock:model"])
        .await?;
    idle(&fixture, &routed).await?;
    fixture
        .run(&["agents", "route", "set", &routed, "mock:alt"])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "route", "show", &routed]).await?["payload"]["state"]["current"],
        "mock:alt"
    );
    fixture
        .rejected(&["agents", "route", "set", &routed, "missing-model"])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "show", &routed]).await?["payload"]["confirmed_route"],
        "mock:alt"
    );
    fixture.run(&["agents", "route", "reset", &routed]).await?;
    assert!(
        fixture.run(&["agents", "route", "show", &routed]).await?["payload"]["state"]["current"]
            .is_null()
    );
    let held_prompt = fixture.prompt("timeout.txt", "wait-for-timeout after new session")?;
    fixture
        .run(&[
            "agents",
            "prompt",
            &routed,
            "--prompt-file",
            text_path(&held_prompt)?,
        ])
        .await?;
    fixture
        .wait(&["agents", "show", &routed], |value| {
            value["payload"]["turn"] == "working"
        })
        .await?;
    fixture.control("disconnect").await?;
    let failed = fixture
        .wait(&["agents", "show", &routed], |value| {
            value["payload"]["process"] == "failed"
        })
        .await?;
    let session = failed["payload"]["native_session_id"].clone();
    ensure!(!failed["payload"]["failure"].is_null());
    fixture.run(&["agents", "stop", &routed]).await?;
    fixture.run(&["agents", "stop", &routed]).await?;
    let retained = fixture.run(&["agents", "show", &routed]).await?;
    assert_eq!(retained["payload"]["native_session_id"], session);
    ensure!(!retained["payload"]["failure"].is_null());
    let unmanaged = fixture.workspace.join("native-saved-session.json");
    std::fs::write(&unmanaged, "harness-owned session")?;
    fixture.run(&["agents", "remove", &routed]).await?;
    assert_eq!(
        std::fs::read_to_string(&unmanaged)?,
        "harness-owned session"
    );
    Ok(())
}

#[tokio::test]
async fn acp_cli_defaults_unsupported_and_permission_timeout_remain_explicit() -> Result<()> {
    let fixture = Fixture::new(Some("overlapping-permissions"), None).await?;
    let denied = fixture
        .output(&[
            "run",
            "stub",
            "ordinary foreground",
            "--direct",
            "--cwd",
            text_path(&fixture.workspace)?,
        ])
        .await?;
    assert_eq!(
        denied.status.code(),
        Some(5),
        "denied foreground must preserve its permission exit code"
    );
    let decisions: Vec<_> = fixture
        .captured()?
        .into_iter()
        .filter(|message| message["result"]["outcome"]["outcome"] == "selected")
        .collect();
    assert_eq!(decisions.len(), 2);
    ensure!(decisions.iter().all(|message| {
        message["result"]["outcome"]["optionId"]
            .as_str()
            .is_some_and(|id| id.starts_with("deny-"))
    }));
    let run = fixture
        .background("bounded Ask", &["--direct", "--turn-timeout", "1"])
        .await?;
    let failed = fixture
        .wait(&["agents", "show", &run], |value| {
            value["payload"]["turn"] == "idle" && value["payload"]["failure"].is_string()
        })
        .await?;
    assert!(
        failed["payload"]["pending_permissions"]
            .as_array()
            .context("pending")?
            .is_empty()
    );
    ensure!(
        failed["payload"]["failure"]
            .as_str()
            .context("failure")?
            .contains("timed out")
    );
    fixture
        .rejected(&["agents", "mode", "set", &run, "plan"])
        .await?;
    fixture
        .rejected(&[
            "agents",
            "config",
            "set",
            &run,
            "missing",
            "--value-json",
            "true",
        ])
        .await?;
    assert_eq!(
        fixture.run(&["agents", "mode", "show", &run]).await?["payload"]["supported"],
        false
    );
    fixture.run(&["agents", "stop", &run]).await?;
    let approved = fixture
        .background("explicit opt in", &["--direct", "--approve-all"])
        .await?;
    assert!(
        idle(&fixture, &approved).await?["payload"]["pending_permissions"]
            .as_array()
            .context("pending")?
            .is_empty()
    );
    let decisions: Vec<_> = fixture
        .captured()?
        .into_iter()
        .filter(|message| {
            message["result"]["outcome"]["optionId"]
                .as_str()
                .is_some_and(|id| id.starts_with("allow-"))
        })
        .collect();
    assert_eq!(decisions.len(), 2);
    Ok(())
}

#[tokio::test]
async fn acp_cli_fresh_session_preserves_direct_routing_and_turn_timeout() -> Result<()> {
    let fixture = Fixture::new(Some("control-settings"), None).await?;
    let run = fixture
        .background("first native session", &["--direct", "--turn-timeout", "1"])
        .await?;
    let first = idle(&fixture, &run).await?;
    fixture.run(&["agents", "session", "new", &run]).await?;
    let fresh = idle(&fixture, &run).await?;
    let session = field(&fresh, "/payload/native_session_id")?;
    assert_ne!(
        fresh["payload"]["native_session_id"],
        first["payload"]["native_session_id"]
    );
    let prompt = fixture.prompt(
        "new-session-timeout.txt",
        "wait-for-timeout in the new session",
    )?;
    fixture
        .run(&[
            "agents",
            "prompt",
            &run,
            "--prompt-file",
            text_path(&prompt)?,
        ])
        .await?;
    fixture
        .wait(&["agents", "show", &run], |value| {
            value["payload"]["failure"]
                .as_str()
                .is_some_and(|failure| failure.contains("timed out"))
        })
        .await?;
    let requests = fixture.captured()?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "session/new")
            .count(),
        2
    );
    assert!(
        requests
            .iter()
            .any(|request| request["method"] == "session/cancel"
                && request["params"]["sessionId"] == session)
    );
    fixture.rejected(&["agents", "route", "show", &run]).await?;
    Ok(())
}

fn model_reply(delta: Value, finish: &str) -> ResponseTemplate {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    );
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

async fn native_thread(fixture: &Fixture, key: &str, extra: &[&str]) -> Result<String> {
    let mut args = vec![
        "threads",
        "create",
        "--workspace",
        text_path(&fixture.workspace)?,
        "--model",
        "mock:model",
        "--request-id",
        key,
    ];
    args.extend_from_slice(extra);
    Ok(field(&fixture.run(&args).await?, "/payload/snapshot/thread_id")?.into())
}

async fn native_start(fixture: &Fixture, thread: &str, prompt: &str, key: &str) -> Result<String> {
    let path = fixture.prompt(&format!("{key}.txt"), prompt)?;
    Ok(field(
        &fixture
            .run(&[
                "threads",
                "start",
                thread,
                "--prompt-file",
                text_path(&path)?,
                "--request-id",
                key,
            ])
            .await?,
        "/payload/receipt/turn_id",
    )?
    .into())
}

async fn native_pending(fixture: &Fixture, thread: &str) -> Result<Value> {
    let view = fixture
        .wait(&["threads", "show", thread], |value| {
            let turn = &value["payload"]["view"]["latest_turn"];
            turn["pending_input"].is_object()
                || matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "cancelled")
                )
        })
        .await?;
    ensure!(
        view["payload"]["view"]["latest_turn"]["pending_input"].is_object(),
        "native turn settled before approval: {view}"
    );
    fixture.run(&["threads", "pending", thread]).await
}

#[tokio::test]
async fn native_cli_steering_and_close_control_the_original_worker_and_queue() -> Result<()> {
    let received = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let handler_requests = requests.clone();
    let handler_received = received.clone();
    let handler_release = release.clone();
    let router = axum::Router::new().route("/chat/completions", axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
        let requests = handler_requests.clone();
        let received = handler_received.clone();
        let release = handler_release.clone();
        async move {
            let first = { let mut requests = requests.lock().await; requests.push(value); requests.len() == 1 };
            if first { received.notify_one(); release.notified().await; }
            let body = format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"index":0,"delta":{"content":"steered result"},"finish_reason":"stop"}]}));
            ([("content-type", "text/event-stream")], body)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move { axum::serve(listener, router).await });
    let fixture = Fixture::new(None, Some(&format!("http://{address}"))).await?;
    let thread = native_thread(&fixture, "create-steer", &[]).await?;
    let turn = native_start(&fixture, &thread, "original bounded request", "held-turn").await?;
    tokio::time::timeout(Duration::from_secs(10), received.notified()).await?;
    let steering = fixture.prompt("steer.txt", "STEERING_FROM_CLI")?;
    let args = [
        "threads",
        "steer",
        &thread,
        "--turn",
        &turn,
        "--prompt-file",
        text_path(&steering)?,
        "--request-id",
        "original-steering",
    ];
    let accepted = fixture.run(&args).await?;
    assert_eq!(fixture.run(&args).await?, accepted);
    fixture
        .rejected(&[
            "threads",
            "steer",
            &thread,
            "--turn",
            "stale-turn",
            "--prompt-file",
            text_path(&steering)?,
        ])
        .await?;
    release.notify_one();
    fixture
        .wait(&["threads", "show", &thread], |value| {
            value["payload"]["view"]["thread"]["active_turn_id"].is_null()
        })
        .await?;
    let history = fixture
        .run(&["threads", "history", &thread, "--limit", "1000"])
        .await?;
    ensure!(history.to_string().contains("STEERING_FROM_CLI"));
    ensure!(
        requests
            .lock()
            .await
            .iter()
            .any(|request| request.to_string().contains("STEERING_FROM_CLI")),
        "accepted steering never reached the model"
    );
    fixture
        .rejected(&[
            "threads",
            "steer",
            &thread,
            "--turn",
            &turn,
            "--prompt-file",
            text_path(&steering)?,
        ])
        .await?;

    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(model_reply(json!({"tool_calls":[{"index":0,"id":"blocked-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"forbidden.txt\",\"content\":\"blocked\"}"}}]}), "tool_calls")).mount(&upstream).await;
    let fixture = Fixture::new(None, Some(&upstream.uri())).await?;
    let thread = native_thread(&fixture, "create-close", &[]).await?;
    let turn = native_start(&fixture, &thread, "hold on approval", "close-active").await?;
    native_pending(&fixture, &thread).await?;
    let queued_prompt = fixture.prompt("close-queued.txt", "never execute queued")?;
    let queued = fixture
        .run(&[
            "threads",
            "enqueue",
            &thread,
            "--prompt-file",
            text_path(&queued_prompt)?,
        ])
        .await?;
    let queued = field(&queued, "/payload/receipt/turn_id")?.to_owned();
    let closed = fixture
        .run(&[
            "threads",
            "close",
            &thread,
            "--request-id",
            "original-close",
        ])
        .await?;
    let replay = fixture
        .run(&[
            "threads",
            "close",
            &thread,
            "--request-id",
            "original-close",
        ])
        .await?;
    assert_eq!(replay["request_id"], closed["request_id"]);
    assert_eq!(
        replay["payload"]["snapshot"]["thread_id"],
        closed["payload"]["snapshot"]["thread_id"]
    );
    assert_eq!(replay["payload"]["snapshot"]["queued"], json!([]));
    assert!(replay["payload"]["snapshot"]["active_turn_id"].is_null());
    for id in [&turn, &queued] {
        assert_eq!(
            fixture
                .run(&["threads", "show", &thread, "--turn", id])
                .await?["payload"]["snapshot"]["status"],
            "cancelled"
        );
    }
    ensure!(!fixture.workspace.join("forbidden.txt").exists());
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        1
    );
    // Close retains a reusable paused Thread. Resuming its emptied queue must
    // not restore either cancelled Turn or repeat an accepted model request.
    fixture.run(&["threads", "resume", &thread]).await?;
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn native_cli_cold_recovery_rejects_unknown_verification_effects_without_replay() -> Result<()>
{
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(model_reply(json!({"content":"ready for check"}), "stop"))
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(None, Some(&upstream.uri())).await?;
    let thread = native_thread(
        &fixture,
        "create-crash",
        &["--check", "printf x >> crash-proof.txt; sleep 2"],
    )
    .await?;
    let turn = native_start(
        &fixture,
        &thread,
        "verification effect crash",
        "start-crash",
    )
    .await?;
    let pending = native_pending(&fixture, &thread).await?;
    fixture
        .run(&[
            "threads",
            "answer",
            &thread,
            "--turn",
            &turn,
            "--request",
            field(&pending, "/payload/pending/request/request_id")?,
            "allow",
        ])
        .await?;
    let marker = fixture.workspace.join("crash-proof.txt");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    fixture.stop(true).await?;
    fixture.start().await?;
    let loaded = fixture
        .run(&[
            "threads",
            "load",
            &thread,
            "--workspace",
            text_path(&fixture.workspace)?,
        ])
        .await?;
    let recovery = &loaded["payload"]["view"]["recovery"];
    ensure!(
        recovery["blockers"]
            .to_string()
            .contains("effect_unconfirmed"),
        "lost verification effect was not preserved: {loaded}"
    );
    ensure!(loaded["payload"]["view"]["latest_turn"]["unknown_effect"] == true);
    let request = fixture.prompt("blocked-recovery.json", &serde_json::to_string(&json!({"source_server_instance_id":recovery["source_server_instance_id"],"source_cursor":recovery["source_cursor"],"idempotency_key":"never-replay"}))?)?;
    fixture
        .rejected(&[
            "threads",
            "recover",
            &thread,
            "--from",
            text_path(&request)?,
        ])
        .await?;
    fixture.rejected(&["threads", "resume", &thread]).await?;
    assert_eq!(std::fs::read_to_string(marker)?, "x");
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn native_cli_ask_fifo_cancel_resume_verification_and_cold_recovery() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(|request: &wiremock::Request| {
        let value = serde_json::from_slice::<Value>(&request.body).unwrap_or(Value::Null);
        let has_result = value["messages"].as_array().is_some_and(|rows| rows.iter().any(|row| row["role"] == "tool"));
        let text = value["messages"].as_array().and_then(|rows| rows.iter().rev().find(|row| row["role"] == "user")).map(|row| row["content"].to_string()).unwrap_or_default();
        if !has_result && (text.contains("CANCEL_PROOF") || text.contains("ALLOW_PROOF")) {
            model_reply(json!({"tool_calls":[{"index":0,"id":"write-once","type":"function","function":{"name":"write","arguments":"{\"path\":\"once.txt\",\"content\":\"approved once\"}"}}]}), "tool_calls")
        } else { model_reply(json!({"content":"native fixture result"}), "stop") }
    }).mount(&upstream).await;
    let mut fixture = Fixture::new(None, Some(&upstream.uri())).await?;
    let thread = native_thread(&fixture, "create-cancel", &[]).await?;
    let turn = native_start(&fixture, &thread, "CANCEL_PROOF", "start-cancel").await?;
    let pending = native_pending(&fixture, &thread).await?;
    let approval = field(&pending, "/payload/pending/request/request_id")?.to_owned();
    assert_eq!(pending["payload"]["pending"]["turn_id"], turn);
    fixture
        .rejected(&[
            "threads",
            "answer",
            &thread,
            "--turn",
            "wrong-turn",
            "--request",
            &approval,
            "allow",
        ])
        .await?;
    fixture
        .rejected(&[
            "threads",
            "answer",
            &thread,
            "--turn",
            &turn,
            "--request",
            "wrong-request",
            "allow",
        ])
        .await?;
    ensure!(!fixture.workspace.join("once.txt").exists());
    let queue = fixture.prompt("queued.txt", "QUEUED_PROOF")?;
    let first = fixture
        .run(&[
            "threads",
            "enqueue",
            &thread,
            "--prompt-file",
            text_path(&queue)?,
            "--request-id",
            "queued-one",
        ])
        .await?;
    let queued = field(&first, "/payload/receipt/turn_id")?.to_owned();
    let second = fixture
        .run(&[
            "threads",
            "enqueue",
            &thread,
            "--prompt-file",
            text_path(&queue)?,
            "--request-id",
            "queued-two",
        ])
        .await?;
    let withdrawn = field(&second, "/payload/receipt/turn_id")?.to_owned();
    assert_ne!(queued, withdrawn);
    // An observation socket can disappear while the approval/worker survives.
    let mut observer = fixture
        .command()
        .args(["threads", "events", &thread, "--follow"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut output = BufReader::new(observer.stdout.take().context("native observer output")?);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line)).await??;
    assert_eq!(
        serde_json::from_str::<Value>(&line)?["kind"],
        "native_event"
    );
    observer.start_kill()?;
    observer.wait().await?;
    assert_eq!(
        fixture.run(&["threads", "pending", &thread]).await?["payload"]["pending"]["request"]["request_id"],
        approval
    );
    fixture
        .run(&[
            "threads",
            "cancel",
            &thread,
            "--turn",
            &withdrawn,
            "--queued",
            "--request-id",
            "withdraw-second",
        ])
        .await?;
    fixture
        .run(&[
            "threads",
            "cancel",
            &thread,
            "--turn",
            &turn,
            "--request-id",
            "cancel-active",
        ])
        .await?;
    fixture
        .wait(&["threads", "show", &thread], |value| {
            value["payload"]["view"]["thread"]["active_turn_id"].is_null()
        })
        .await?;
    fixture
        .rejected(&[
            "threads",
            "answer",
            &thread,
            "--turn",
            &turn,
            "--request",
            &approval,
            "allow",
        ])
        .await?;
    let paused = fixture.run(&["threads", "show", &thread]).await?;
    assert_eq!(
        paused["payload"]["view"]["thread"]["queued"][0]["turn_id"],
        queued
    );
    ensure!(!fixture.workspace.join("once.txt").exists());
    fixture
        .run(&["threads", "resume", &thread, "--request-id", "resume-queue"])
        .await?;
    fixture
        .wait(&["threads", "show", &thread, "--turn", &queued], |value| {
            value["payload"]["snapshot"]["status"] == "completed"
        })
        .await?;
    assert_eq!(
        fixture
            .run(&["threads", "show", &thread, "--turn", &withdrawn])
            .await?["payload"]["snapshot"]["status"],
        "cancelled"
    );
    let receipt = fixture
        .run(&[
            "threads",
            "enqueue",
            &thread,
            "--prompt-file",
            text_path(&queue)?,
            "--request-id",
            "queued-one",
        ])
        .await?;
    assert_eq!(receipt["payload"]["receipt"]["turn_id"], queued);
    fixture
        .rejected(&[
            "threads",
            "resume",
            &thread,
            "--server-instance",
            "old-runtime",
        ])
        .await?;
    let positive =
        native_thread(&fixture, "create-allow", &["--check", "test -f once.txt"]).await?;
    let approved_turn = native_start(&fixture, &positive, "ALLOW_PROOF", "start-allow").await?;
    let pending = native_pending(&fixture, &positive).await?;
    let request = field(&pending, "/payload/pending/request/request_id")?.to_owned();
    fixture
        .run(&[
            "threads",
            "answer",
            &positive,
            "--turn",
            &approved_turn,
            "--request",
            &request,
            "allow",
            "--request-id",
            "approve-once",
        ])
        .await?;
    let verification = fixture
        .wait(&["threads", "pending", &positive], |value| {
            value["payload"]["pending"].is_object()
                && value["payload"]["pending"]["request"]["request_id"] != request
        })
        .await?;
    let verification_request =
        field(&verification, "/payload/pending/request/request_id")?.to_owned();
    ensure!(
        verification["payload"]["pending"]["request"]["arguments"]
            .to_string()
            .contains("test -f once.txt"),
        "verification approval lost the exact invocation"
    );
    fixture
        .run(&[
            "threads",
            "answer",
            &positive,
            "--turn",
            &approved_turn,
            "--request",
            &verification_request,
            "allow",
            "--request-id",
            "approve-verification",
        ])
        .await?;
    let complete = fixture
        .wait(
            &["threads", "show", &positive, "--turn", &approved_turn],
            |value| value["payload"]["snapshot"]["status"] == "completed",
        )
        .await?;
    assert_eq!(complete["payload"]["snapshot"]["verification"], "passed");
    assert_eq!(
        complete["payload"]["snapshot"]["verification_evidence"]["command"],
        "test -f once.txt"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("once.txt"))?,
        "approved once"
    );
    fixture
        .rejected(&[
            "threads",
            "answer",
            &positive,
            "--turn",
            &approved_turn,
            "--request",
            &request,
            "allow",
            "--request-id",
            "different-answer",
        ])
        .await?;
    fixture.rejected(&["agents", "show", &positive]).await?;
    let read_only = native_thread(&fixture, "readonly", &["--read-only"]).await?;
    assert_eq!(
        fixture.run(&["threads", "show", &read_only]).await?["payload"]["view"]["thread"]["permission_profile"],
        "read_only"
    );
    fixture
        .rejected(&[
            "threads",
            "create",
            "--workspace",
            text_path(&fixture.workspace)?,
            "--model",
            "mock:model",
            "--read-only",
            "--check",
            "echo forbidden",
        ])
        .await?;
    fixture
        .run(&[
            "threads",
            "close",
            &read_only,
            "--request-id",
            "close-readonly",
        ])
        .await?;
    // Durable paged history is distinct from the bounded observation replay.
    let mut after = 0_u64;
    let mut cutoff = None;
    let mut events = Vec::new();
    loop {
        let after_text = after.to_string();
        let cutoff_text = cutoff.map(|value: u64| value.to_string());
        let mut args = vec![
            "threads",
            "history",
            &thread,
            "--after",
            &after_text,
            "--limit",
            "1",
        ];
        if let Some(cutoff) = &cutoff_text {
            args.extend(["--cutoff", cutoff]);
        }
        let page = fixture.run(&args).await?;
        cutoff = page["payload"]["page"]["cutoff"].as_u64();
        events.extend(
            page["payload"]["page"]["events"]
                .as_array()
                .context("history events")?
                .clone(),
        );
        let Some(next) = page["payload"]["page"]["next_after"].as_u64() else {
            break;
        };
        ensure!(next > after, "history cursor did not progress");
        after = next;
    }
    let history = serde_json::to_string(&events)?;
    ensure!(history.contains(&turn) && history.contains(&queued) && history.contains(&withdrawn));
    fixture
        .wait(&["threads", "show", &positive], |value| {
            value["payload"]["view"]["thread"]["active_turn_id"].is_null()
        })
        .await?;
    fixture.run(&["threads", "unload", &positive]).await?;
    let cold = fixture.run(&["threads", "show", &positive]).await?;
    assert_eq!(
        cold["payload"]["view"]["latest_turn"]["turn_id"],
        approved_turn
    );
    let calls_before = upstream
        .received_requests()
        .await
        .context("model requests")?
        .len();
    fixture.stop(false).await?;
    fixture.start().await?;
    fixture.run(&["threads", "list", "--limit", "1"]).await?;
    let loaded = fixture
        .run(&[
            "threads",
            "load",
            &positive,
            "--workspace",
            text_path(&fixture.workspace)?,
        ])
        .await?;
    let recovery = &loaded["payload"]["view"]["recovery"];
    ensure!(
        recovery.is_object(),
        "cold load omitted its recovery inspection: {loaded}"
    );
    let accepted = json!({"source_server_instance_id":recovery["source_server_instance_id"],"source_cursor":recovery["source_cursor"],"idempotency_key":"recover-original"});
    let request_file = fixture.directory.path().join("recovery.json");
    std::fs::write(&request_file, serde_json::to_vec(&accepted)?)?;
    let mut stale = accepted.clone();
    stale["source_cursor"] = json!(0);
    let stale_file = fixture.directory.path().join("stale-recovery.json");
    std::fs::write(&stale_file, serde_json::to_vec(&stale)?)?;
    fixture
        .rejected(&[
            "threads",
            "recover",
            &positive,
            "--from",
            text_path(&stale_file)?,
        ])
        .await?;
    fixture
        .run(&[
            "threads",
            "recover",
            &positive,
            "--from",
            text_path(&request_file)?,
        ])
        .await?;
    fixture
        .run(&[
            "threads",
            "recover",
            &positive,
            "--from",
            text_path(&request_file)?,
        ])
        .await?;
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("model requests")?
            .len(),
        calls_before
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("once.txt"))?,
        "approved once"
    );
    Ok(())
}

#[tokio::test]
async fn removed_ui_entrypoints_leave_live_runs_configuration_and_history_intact() -> Result<()> {
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let run = fixture
        .background("Retain this accepted work", &["--direct"])
        .await?;
    let before = idle(&fixture, &run).await?;
    let configuration = std::fs::read(&fixture.config)?;
    let captured = fixture.captured()?;
    for args in [
        vec!["code"],
        vec!["tui", "stub", "--direct"],
        vec!["chat", "stub", "--direct", "--help"],
        vec![
            "--context",
            "missing-context",
            "code",
            "--socket",
            "/no/control/socket",
        ],
        vec!["--context", "missing-context", "agents", "attach", &run],
    ] {
        let error = fixture.rejected(&args).await?;
        ensure!(
            error.to_string().contains("removed"),
            "missing migration guidance for {args:?}: {error}"
        );
        ensure!(
            !error.to_string().contains("missing-context"),
            "retired UI reached context resolution: {error}"
        );
    }
    let human = fixture
        .command()
        .args(["--human", "code", "stub"])
        .output()
        .await?;
    ensure!(!human.status.success());
    ensure!(String::from_utf8_lossy(&human.stdout).contains("bro run"));
    ensure!(
        !human.stdout.contains(&0x1b),
        "retired UI entered terminal mode"
    );
    for args in [vec!["--help"], vec!["agents", "--help"]] {
        let help = fixture.command().args(args).output().await?;
        ensure!(help.status.success());
        for line in String::from_utf8_lossy(&help.stdout).lines() {
            let name = line.split_whitespace().next().unwrap_or_default();
            ensure!(
                !["code", "tui", "chat", "attach"].contains(&name),
                "retired command remains in help: {line}"
            );
        }
    }
    let after = fixture.run(&["agents", "show", &run]).await?;
    for key in [
        "run_id",
        "process",
        "turn",
        "review",
        "native_session_id",
        "last_seq",
    ] {
        assert_eq!(
            before["payload"][key], after["payload"][key],
            "UI migration changed retained {key}"
        );
    }
    assert_eq!(configuration, std::fs::read(&fixture.config)?);
    assert_eq!(captured, fixture.captured()?);
    Ok(())
}

#[tokio::test]
async fn legacy_chat_defaults_keep_headless_expansion_and_report_ui_migration() -> Result<()> {
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let mut config: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    config["chat"] = json!({"agent":"stub", "model":"legacy-default-model", "commands":[{"name":"review","description":"Headless review template","prompt":"Review $ARGUMENTS"}]});
    let saved = serde_json::to_vec_pretty(&config)?;
    std::fs::write(&fixture.config, &saved)?;
    let keymap = fixture
        .directory
        .path()
        .join(".config/bitrouter/code-hotkeys.json");
    std::fs::create_dir_all(keymap.parent().context("keymap parent")?)?;
    let old_bindings = b"{\"F2\":\"review_permission\"}\n";
    std::fs::write(&keymap, old_bindings)?;
    let validate = fixture
        .run(&[
            "config",
            "validate",
            "--config",
            text_path(&fixture.config)?,
        ])
        .await?;
    assert_eq!(validate["valid"], true);
    ensure!(
        validate["migration_hint"]
            .as_str()
            .context("migration hint")?
            .contains("chat.model and chat.commands remain")
    );
    let output = fixture
        .command()
        .args([
            "run",
            "stub",
            "/review this diff",
            "--direct",
            "--format",
            "text",
            "--config",
        ])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "headless defaults failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !output.stdout.contains(&0x1b),
        "plain transcript contains terminal controls"
    );
    let prompts: Vec<_> = fixture
        .captured()?
        .into_iter()
        .filter(|entry| entry["method"] == "session/prompt")
        .collect();
    ensure!(
        prompts
            .iter()
            .any(|entry| entry["params"]["prompt"][0]["text"] == "Review this diff"),
        "headless template did not reach the peer: {prompts:?}"
    );
    assert_eq!(std::fs::read(&fixture.config)?, saved);
    assert_eq!(std::fs::read(&keymap)?, old_bindings);
    Ok(())
}

#[tokio::test]
async fn protocol_attachment_keeps_fenced_history_heartbeat_and_release_without_ui() -> Result<()> {
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let run = fixture
        .background("Retain protocol attachment", &["--direct"])
        .await?;
    let before = idle(&fixture, &run).await?;
    let owner = bitrouter::agent_sessions::BackgroundClient::new(fixture.socket.clone());
    let attachment = owner.attach(&run, false).await?;
    assert_eq!(attachment.snapshot.run_id, run);
    ensure!(
        !attachment.replay.events.is_empty(),
        "attachment lost retained history"
    );
    let generation = attachment
        .snapshot
        .lease
        .as_ref()
        .context("attached lease")?
        .generation;
    tokio::time::sleep(Duration::from_secs(6)).await;
    owner.heartbeat_due_leases().await?;
    let held = owner
        .runs(None)
        .await?
        .into_iter()
        .find(|snapshot| snapshot.run_id == run)
        .context("attached run")?;
    assert_eq!(
        held.lease.as_ref().context("renewed lease")?.generation,
        generation
    );
    assert_eq!(
        held.native_session_id.as_deref(),
        before["payload"]["native_session_id"].as_str()
    );
    owner.release_all().await;
    let released = fixture.run(&["agents", "show", &run]).await?;
    assert!(released["payload"]["lease"].is_null());
    assert_eq!(released["payload"]["process"], "running");
    assert_eq!(released["payload"]["review"], "unread");
    Ok(())
}
