use super::*;
use bitrouter_sdk::mcp::transport::McpServerConfig;

#[test]
fn selected_descriptors_preserve_config_without_exposing_the_other_servers() -> Result<()> {
    let mut config = Config::default();
    config.mcp_servers.clear();
    config.mcp_servers.insert(
        "selected".into(),
        McpServerConfig::with_defaults(
            "selected",
            McpTransport::Stdio {
                command: "resource-fixture".into(),
                args: vec!["literal argument".into()],
                env: std::collections::HashMap::from([
                    ("Z_LAST".into(), "private fixture credential".into()),
                    ("A_FIRST".into(), "value".into()),
                ]),
            },
        ),
    );
    config.mcp_servers.insert(
        "other".into(),
        McpServerConfig::with_defaults(
            "other",
            McpTransport::Http {
                url: "https://example.test/mcp".into(),
                headers: Default::default(),
            },
        ),
    );
    let selection = Selection {
        mcp_servers: vec!["selected".into()],
        ..Default::default()
    };
    let wire = serde_json::to_value(servers(&config, &selection)?)?;
    assert_eq!(wire.as_array().map(Vec::len), Some(1));
    assert_eq!(wire[0]["name"], "selected");
    assert_eq!(wire[0]["args"][0], "literal argument");
    assert_eq!(wire[0]["env"][0]["name"], "A_FIRST");
    assert_eq!(wire[0]["env"][1]["value"], "private fixture credential");
    let before = digest(&serde_json::to_vec(&servers(&config, &selection)?)?);
    if let Some(server) = config.mcp_servers.get_mut("selected") {
        server.transport = McpTransport::Stdio {
            command: "resource-fixture".into(),
            args: vec!["literal argument".into()],
            env: std::collections::HashMap::from([
                ("A_FIRST".into(), "value".into()),
                ("Z_LAST".into(), "private fixture credential".into()),
            ]),
        };
    }
    assert_eq!(
        digest(&serde_json::to_vec(&servers(&config, &selection)?)?),
        before
    );
    assert!(
        servers(
            &config,
            &Selection {
                mcp_servers: vec!["missing".into()],
                ..Default::default()
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn context_capture_is_bounded_and_rejects_escape_and_non_files() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("nested"))?;
    let text = "  selected context 中文 👩‍💻\nsecond line  ";
    std::fs::write(root.path().join("nested/context.md"), text)?;
    assert_eq!(
        read_bounded(root.path(), Path::new("nested/context.md"))?,
        text
    );
    assert!(read_bounded(root.path(), Path::new("../outside")).is_err());
    assert!(read_bounded(root.path(), Path::new("/outside")).is_err());
    assert!(read_bounded(root.path(), Path::new("nested")).is_err());
    std::fs::write(
        root.path().join("large"),
        vec![b'a'; MAX_MATERIAL_BYTES + 1],
    )?;
    assert!(read_bounded(root.path(), Path::new("large")).is_err());
    std::fs::write(root.path().join("binary"), [0xff])?;
    assert!(read_bounded(root.path(), Path::new("binary")).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn context_capture_refuses_symlink_components_and_special_files() -> Result<()> {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    std::fs::write(other.path().join("private"), "outside selected scope")?;
    symlink(other.path(), root.path().join("alias"))?;
    symlink(other.path().join("private"), root.path().join("leaf"))?;
    assert!(read_bounded(root.path(), Path::new("alias/private")).is_err());
    assert!(read_bounded(root.path(), Path::new("leaf")).is_err());
    nix::unistd::mkfifo(
        &root.path().join("pipe"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )?;
    assert!(read_bounded(root.path(), Path::new("pipe")).is_err());
    Ok(())
}

#[tokio::test]
async fn advertised_skills_have_stable_distinct_ids_and_validation_rejects_invention() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let roots = [root.path().join("first"), root.path().join("second")];
    for selected_root in &roots {
        let path = selected_root.join("skills/same-name");
        std::fs::create_dir_all(&path)?;
        std::fs::write(
            path.join("SKILL.md"),
            "---\nname: same-name\ndescription: Test selected material\n---\nConcrete instruction\n",
        )?;
    }
    let catalog = SkillCatalog::discover(&roots)?;
    assert_eq!(catalog.skills.len(), 2);
    assert_ne!(
        catalog.skills[0].material.material_id,
        catalog.skills[1].material.material_id
    );
    let inventory = Inventory {
        model_routes: Vec::new(),
        route_control_harnesses: Vec::new(),
        mcp_servers: vec!["known".into()],
        skills: catalog.skills,
        skill_problems: catalog.problems,
        roots: vec![root.path().to_owned()],
    };
    inventory.validate(&Selection {
        skills: vec![bitrouter_orchestrator::goal::resources::SkillSelection {
            material_id: inventory.skills[0].material.material_id.clone(),
            sha256: inventory.skills[0].material.sha256.clone(),
        }],
        ..Default::default()
    })?;
    assert!(
        inventory
            .validate(&Selection {
                skills: vec![bitrouter_orchestrator::goal::resources::SkillSelection {
                    material_id: "same-name".into(),
                    sha256: "a".repeat(64)
                }],
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        inventory
            .validate(&Selection {
                mcp_servers: vec!["unknown".into()],
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        inventory
            .validate(&Selection {
                skills: vec![bitrouter_orchestrator::goal::resources::SkillSelection {
                    material_id: inventory.skills[0].material.material_id.clone(),
                    sha256: "0".repeat(64),
                }],
                ..Default::default()
            })
            .is_err(),
        "changed skill versions must require a fresh selection"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unsupported_mcp_transports_are_rejected_before_session_open_and_reaped() -> Result<()> {
    startup_rejection(false).await
}

#[cfg(unix)]
#[tokio::test]
async fn unconfirmed_model_route_never_prompts_and_reaps_the_actual_adapter() -> Result<()> {
    startup_rejection(true).await
}

#[cfg(unix)]
async fn startup_rejection(model_route: bool) -> Result<()> {
    use crate::acp_cli::{
        LaunchOptions, RoutingOptions, SessionHost, SessionSelection, SpawnContext,
    };
    use bitrouter_sdk::config::agent::{AcpAgentConfig, AcpTransport};
    let binary = super::super::tests::peer_executable()?;
    let descriptors = if model_route {
        vec![None]
    } else {
        vec![
            Some(acp::McpServer::Http(acp::McpServerHttp::new(
                "selected",
                "https://example.test/mcp",
            ))),
            Some(acp::McpServer::Sse(acp::McpServerSse::new(
                "selected",
                "https://example.test/mcp",
            ))),
        ]
    };
    for descriptor in descriptors {
        let directory = tempfile::tempdir()?;
        let captures = directory.path().join("captures");
        let controls = directory.path().join("controls");
        std::fs::create_dir(&captures)?;
        std::fs::create_dir(&controls)?;
        let mut config = Config::default();
        config.agents.insert(
            "resource-peer".into(),
            AcpAgentConfig {
                name: "resource-peer".into(),
                transport: AcpTransport::Stdio {
                    command: binary.to_string_lossy().into_owned(),
                    args: vec![],
                    env: std::collections::HashMap::from([
                        (
                            "BRO_GOAL_FIXTURE_CONTROL".into(),
                            controls.to_string_lossy().into_owned(),
                        ),
                        (
                            "BRO_GOAL_FIXTURE_CAPTURE".into(),
                            captures.to_string_lossy().into_owned(),
                        ),
                    ]),
                },
            },
        );
        let source = crate::paths::ConfigSource::Default {
            home: directory.path().to_owned(),
        };
        let host = SessionHost::prepare(
            SpawnContext {
                source: &source,
                config,
                agent_id: "resource-peer",
                options: LaunchOptions {
                    mcp_servers: descriptor.into_iter().collect(),
                    required_session_route: model_route.then(|| "goal-model".into()),
                    ..Default::default()
                },
                routing: RoutingOptions {
                    direct: true,
                    ..Default::default()
                },
            },
            false,
        )
        .await?;
        let result = host
            .open(&SessionSelection::New, directory.path().to_owned())
            .await;
        let error = match result {
            Ok(mut handle) => {
                handle.shutdown().await;
                anyhow::bail!("unsupported transport opened a session");
            }
            Err(error) => error,
        };
        ensure!(error.to_string().contains(if model_route {
            "selected BRO model route was not confirmed"
        } else {
            "not supported"
        }));
        let files = std::fs::read_dir(&captures)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].path())?;
        let messages = text
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["method"] == "initialize")
                .count(),
            1
        );
        assert!(
            !messages
                .iter()
                .any(|message| message["method"] == "session/prompt")
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["method"] == "session/new")
                .count(),
            usize::from(model_route)
        );
        assert!(!error.to_string().contains("cleanup did not confirm"));
    }
    Ok(())
}
