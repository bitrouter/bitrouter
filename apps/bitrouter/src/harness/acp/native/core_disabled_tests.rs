//! Test both negotiated ACP versions without acquiring native execution rights.

use crate::harness::agent::AgentConfigExt;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::harness::service::core_disabled_tests::offline_fixture;
use crate::harness::storage::ExecutionStore;

#[tokio::test]
async fn native_creation_is_rejected_before_workspace_registration_in_both_versions()
-> Result<(), Box<dyn std::error::Error>> {
    for version in [1, 2] {
        let fixture = offline_fixture(CallerContext::local()).await?;
        let before = serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?;
        let server = NativeAcpServer::new(
            fixture.service.clone(),
            CallerContext::local(),
            NativeSessionConfig {
                agent: AgentConfig::fixed("fixture", None),
                permission_profile: PermissionProfile::ReadOnly,
                servers: Vec::new(),
                register_local_workspaces: true,
            },
        );
        let (mut client, stream) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(stream);
        let connected = server.clone();
        let task = tokio::spawn(async move { connected.connect(read, write).await });
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":version,"info":{"name":"offline-test","version":"1"},
            "clientInfo":{"name":"offline-test","version":"1"}
        }});
        let new = json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{
            "cwd":fixture.workspace.path().join("must-not-be-discovered"),"mcpServers":[]
        }});
        for (id, request) in [(1, initialize), (2, new)] {
            let mut bytes = serde_json::to_vec(&request)?;
            bytes.push(b'\n');
            client.write_all(&bytes).await?;
            let mut line = String::new();
            tokio::time::timeout(
                Duration::from_secs(3),
                BufReader::new(&mut client).read_line(&mut line),
            )
            .await??;
            let reply: Value = serde_json::from_str(&line)?;
            assert_eq!(reply["id"], id);
            if id == 1 {
                assert_eq!(
                    reply["result"]["_meta"]["bitrouter"]["nativeExecution"],
                    false
                );
                assert!(
                    reply["result"]["_meta"]["bitrouter"]
                        .get("queueResume")
                        .is_none()
                );
                if version == 1 {
                    assert!(
                        reply["result"]["agentCapabilities"]["sessionCapabilities"]
                            .get("close")
                            .is_none()
                    );
                }
            } else {
                assert_eq!(
                    reply["error"]["data"]["bitrouter"]["code"],
                    "unsupported_capability"
                );
            }
        }
        drop(client);
        tokio::time::timeout(Duration::from_secs(3), task).await???;
        fixture.service.shutdown().await;
        server.shutdown().await;
        assert_eq!(
            before,
            serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?
        );
        assert_eq!(std::fs::read_dir(fixture.workspace.path())?.count(), 0);
    }
    Ok(())
}
