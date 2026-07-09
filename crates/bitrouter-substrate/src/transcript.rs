//! Durable, non-lossy session transcript.
//!
//! One NDJSON file per session at
//! `<base_repo>/.bitrouter/sessions/<record_id>.transcript.ndjson`, appended by
//! a single writer task. Unlike the UI-facing update broadcasts (bounded,
//! lossy under lag), the transcript feed is **unbounded**: every event reaches
//! disk in order.
//!
//! Each line is `{"seq":N,"ts":<unix_ms>,"kind":…,…}`. `seq` is minted by the
//! single writer, so it is strictly monotonic per session — exactly the shape a
//! future `session/resume { replayFrom: <cursor> }` (ACP v2 RFD) needs for
//! replay-then-live-tail handoff: replay the file up to `seq`, then subscribe
//! from `seq + 1`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client_protocol_schema::v1::{ContentBlock, SessionUpdate, StopReason};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

/// One transcript entry, before the writer stamps `seq` and `ts`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// A prompt turn was submitted (the manager's content blocks, verbatim).
    Prompt { blocks: Vec<ContentBlock> },
    /// A raw upstream `session/update`, verbatim. Boxed to keep the enum's
    /// variants comparably sized (`SessionUpdate` is large).
    Update { update: Box<SessionUpdate> },
    /// A prompt turn resolved.
    Result { stop_reason: StopReason },
    /// A prompt turn failed (transport/pipeline error, timeout, …).
    Error { message: String },
}

/// The stamped line as it appears on disk.
#[derive(Serialize)]
struct TranscriptLine<'a> {
    seq: u64,
    /// Unix milliseconds.
    ts: u64,
    #[serde(flatten)]
    event: &'a TranscriptEvent,
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The transcript file path for `record_id` under `base_repo`.
pub fn transcript_path(base_repo: &std::path::Path, record_id: &str) -> PathBuf {
    base_repo
        .join(".bitrouter")
        .join("sessions")
        .join(format!("{record_id}.transcript.ndjson"))
}

/// Spawn the single writer task: drains `rx`, stamps each event with a
/// monotonic `seq` and a timestamp, and appends it as one NDJSON line to
/// `path`. Ends when every sender is dropped; the returned handle lets the
/// session await a complete flush at shutdown.
///
/// Write failures are logged (once per event) rather than propagated — a full
/// disk must not take the live session down.
pub fn spawn_writer(path: PathBuf, mut rx: UnboundedReceiver<TranscriptEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Some(dir) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(dir).await
        {
            tracing::warn!(error = %e, dir = %dir.display(), "cannot create transcript dir; transcript disabled");
            // Drain so senders never block on a closed channel semantics
            // (unbounded senders don't block, but dropping rx would make their
            // sends error-noise; consuming keeps them cheap no-ops).
            while rx.recv().await.is_some() {}
            return;
        }
        let file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "cannot open transcript; transcript disabled");
                while rx.recv().await.is_some() {}
                return;
            }
        };
        let mut out = tokio::io::BufWriter::new(file);
        let mut seq: u64 = 0;
        while let Some(event) = rx.recv().await {
            let line = TranscriptLine {
                seq,
                ts: now_unix_millis(),
                event: &event,
            };
            seq += 1;
            match serde_json::to_string(&line) {
                Ok(mut json) => {
                    json.push('\n');
                    if let Err(e) = out.write_all(json.as_bytes()).await {
                        tracing::warn!(error = %e, "transcript write failed");
                    }
                    // Flush per event: transcripts are the durable record and
                    // event rates are human-scale, so latency beats batching.
                    if let Err(e) = out.flush().await {
                        tracing::warn!(error = %e, "transcript flush failed");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "transcript serialise failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::TextContent;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    #[tokio::test]
    async fn writer_appends_stamped_ordered_lines() {
        let base = tempfile::tempdir().expect("tempdir");
        let path = transcript_path(base.path(), "r1");
        let (tx, rx) = unbounded_channel();
        let writer = spawn_writer(path.clone(), rx);

        tx.send(TranscriptEvent::Prompt {
            blocks: vec![ContentBlock::Text(TextContent::new("hi".to_string()))],
        })
        .expect("send");
        tx.send(TranscriptEvent::Result {
            stop_reason: StopReason::EndTurn,
        })
        .expect("send");
        drop(tx);
        writer.await.expect("writer ends when senders drop");

        let raw = std::fs::read_to_string(&path).expect("transcript file");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid json line"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["seq"], 0);
        assert_eq!(lines[0]["kind"], "prompt");
        assert_eq!(lines[1]["seq"], 1);
        assert_eq!(lines[1]["kind"], "result");
        assert_eq!(lines[1]["stop_reason"], "end_turn");
        assert!(lines[0]["ts"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn writer_appends_across_reopen() {
        // Two writer lifetimes on the same path append rather than truncate —
        // the shape a resumed (v2 warm) session needs. `seq` restarts per
        // writer in v1; a resume implementation seeds it from the file tail.
        let base = tempfile::tempdir().expect("tempdir");
        let path = transcript_path(base.path(), "r1");
        for _ in 0..2 {
            let (tx, rx) = unbounded_channel();
            let writer = spawn_writer(path.clone(), rx);
            tx.send(TranscriptEvent::Result {
                stop_reason: StopReason::EndTurn,
            })
            .expect("send");
            drop(tx);
            writer.await.expect("writer");
        }
        let raw = std::fs::read_to_string(&path).expect("transcript file");
        assert_eq!(raw.lines().count(), 2, "second writer must append");
    }
}
