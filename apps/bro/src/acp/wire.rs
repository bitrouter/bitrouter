//! A sequential ACP v1 stdio peer. Credential-bearing provider RPCs never enter a protocol logger.
use super::translate::translate;
use agent_client_protocol_schema::v1::*;
use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::io::Write;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

pub(super) struct Peer<'a> {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    out: &'a mut dyn Write,
    token: &'a str,
    next_id: u64,
    pub session: Option<SessionId>,
    pub denied: bool,
}

impl<'a> Peer<'a> {
    pub fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
        out: &'a mut dyn Write,
        token: &'a str,
    ) -> Self {
        Self {
            stdin,
            stdout: BufReader::new(stdout),
            out,
            token,
            next_id: 0,
            session: None,
            denied: false,
        }
    }

    async fn send(&mut self, value: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .context("writing to ACP adapter")?;
        self.stdin.flush().await.context("flushing ACP request")
    }

    pub async fn cancel(&mut self) -> Result<()> {
        if let Some(session) = &self.session {
            self.send(
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn request<T: DeserializeOwned>(
        &mut self,
        method: &'static str,
        params: impl Serialize,
    ) -> Result<T> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        const MAX_FRAME: usize = 8 * 1024 * 1024;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut self.stdout)
                .take((MAX_FRAME + 1) as u64)
                .read_until(b'\n', &mut line)
                .await?;
            ensure!(n != 0, "ACP adapter disconnected during {method}");
            ensure!(n <= MAX_FRAME, "ACP frame exceeds size limit");
            let msg: Value =
                serde_json::from_slice(&line).map_err(|_| anyhow::anyhow!("malformed ACP JSON"))?;
            ensure!(msg["jsonrpc"] == "2.0", "invalid ACP envelope");
            if let Some(incoming) = msg.get("method") {
                match (incoming.as_str(), msg.get("id")) {
                    (Some("session/update"), None) => {
                        let notice: SessionNotification = decode(msg["params"].clone())?;
                        if Some(&notice.session_id) == self.session.as_ref()
                            && let Some(event) = translate(notice.update)
                        {
                            self.emit(serde_json::to_value(event)?)?;
                        }
                    }
                    (Some("session/request_permission"), Some(request_id)) => {
                        self.deny(request_id, msg["params"].clone()).await?;
                    }
                    (_, Some(request_id)) => {
                        self.send(json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"method not supported"}})).await?;
                    }
                    (_, None) => {}
                }
                continue;
            }
            ensure!(msg["id"] == id, "unexpected ACP response id");
            if msg.get("error").is_some() {
                bail!("ACP adapter rejected {method}");
            }
            return decode(
                msg.get("result")
                    .context("missing ACP response result")?
                    .clone(),
            );
        }
    }

    async fn deny(&mut self, id: &Value, params: Value) -> Result<()> {
        let request: RequestPermissionRequest = decode(params)?;
        let matching_session = Some(&request.session_id) == self.session.as_ref();
        // An adapter is allowed to offer only allow options. Never select the first as a fallback.
        let reject = request
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectOnce)
            .or_else(|| {
                request
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::RejectAlways)
            });
        let outcome = match reject.filter(|_| matching_session) {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
            None => RequestPermissionOutcome::Cancelled,
        };
        self.send(
            json!({"jsonrpc":"2.0","id":id,"result":RequestPermissionResponse::new(outcome)}),
        )
        .await?;
        if matching_session {
            self.denied = true;
            self.emit(json!({"type":"permission","decision":"denied","title":request.tool_call.fields.title,"kind":request.tool_call.fields.kind}))?;
        }
        Ok(())
    }

    pub fn emit(&mut self, mut event: Value) -> Result<()> {
        redact(&mut event, self.token);
        serde_json::to_writer(&mut *self.out, &event)?;
        writeln!(self.out)?;
        self.out.flush()?;
        Ok(())
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| anyhow::anyhow!("invalid ACP message body"))
}

pub(super) fn redact(value: &mut Value, token: &str) {
    match value {
        Value::String(s) if s.contains(token) => *s = s.replace(token, "[REDACTED]"),
        Value::Object(fields) => fields.values_mut().for_each(|v| redact(v, token)),
        Value::Array(items) => items.iter_mut().for_each(|v| redact(v, token)),
        _ => {}
    }
}
