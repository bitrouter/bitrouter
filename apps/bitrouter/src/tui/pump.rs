//! Per-session pump: forward a `Session`'s update/permission streams into the
//! loop's `Incoming` channel, tagged with the session's `record_id`.

use std::sync::Arc;

use bitrouter_substrate::engine::Session;
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::tui::event::Incoming;

/// Spawn background tasks that pump `session`'s streams into `tx`. The tasks live
/// until the streams end (session shutdown drops them).
pub fn spawn(session: Arc<Session>, record_id: String, tx: UnboundedSender<Incoming>) {
    // Updates.
    {
        let mut updates = session.updates();
        let tx = tx.clone();
        let record_id = record_id.clone();
        tokio::spawn(async move {
            while let Some(update) = updates.next().await {
                if tx
                    .send(Incoming::Update {
                        record_id: record_id.clone(),
                        update,
                    })
                    .is_err()
                {
                    break;
                }
            }
            let _ = tx.send(Incoming::Exited { record_id });
        });
    }
    // Permissions.
    {
        let mut perms = session.permissions();
        let tx = tx.clone();
        let record_id = record_id.clone();
        tokio::spawn(async move {
            while let Some(pending) = perms.next().await {
                if tx
                    .send(Incoming::Permission {
                        record_id: record_id.clone(),
                        pending: Box::new(pending),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport, ConfigAcpRoutingTable};
    use std::collections::HashMap;
    use tokio::sync::mpsc::unbounded_channel;

    // Same bash stub the substrate engine tests use: ACP handshake + one streamed
    // message chunk + a prompt result.
    const BASH_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}\n';
                              printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    fn stub_catalog() -> ConfigAcpRoutingTable {
        let cfg = AcpAgentConfig {
            name: "stub".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), BASH_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        ConfigAcpRoutingTable::from_configs([("stub".to_string(), cfg)]).expect("catalog")
    }

    #[tokio::test]
    async fn pump_forwards_a_message_update() {
        let base = tempfile::tempdir().expect("tempdir");
        let session = Session::launch(
            &stub_catalog(),
            "stub",
            base.path().to_path_buf(),
            bitrouter_substrate::engine::LaunchOptions::default(),
        )
        .await
        .expect("launch");
        let session = Arc::new(session);

        let (tx, mut rx) = unbounded_channel();
        spawn(Arc::clone(&session), "rec-1".to_string(), tx);

        let resp = session.prompt("hi").await.expect("prompt");
        assert_eq!(format!("{:?}", resp.stop_reason), "EndTurn");

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("no timeout")
            .expect("an Incoming");
        match got {
            Incoming::Update { record_id, update } => {
                assert_eq!(record_id, "rec-1");
                assert!(
                    format!("{update:?}").contains("hi"),
                    "unexpected update: {update:?}"
                );
            }
            _ => panic!("expected Update, got a different Incoming variant"),
        }

        Arc::try_unwrap(session)
            .ok()
            .expect("sole owner")
            .shutdown()
            .await
            .expect("shutdown");
    }
}
