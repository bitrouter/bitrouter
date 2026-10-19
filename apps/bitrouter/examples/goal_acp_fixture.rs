//! Cargo-built ACP peer: real files and permission RPC, deterministic test work.
use std::collections::BTreeSet;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

type FixtureResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct State {
    provider: Option<agent_client_protocol::schema::v1::SetProviderRequest>,
    cwd: PathBuf,
    pending: Option<(Value, String)>,
    permissions: BTreeSet<String>,
}
struct Peer {
    state: Mutex<State>,
    output: Mutex<()>,
    cancelled: AtomicBool,
    control: PathBuf,
    session: String,
    omit_tool_completion: bool,
    right_manual: bool,
    right_gate: bool,
    right_overlap: bool,
}

fn lock<T>(value: &Mutex<T>) -> FixtureResult<std::sync::MutexGuard<'_, T>> {
    Ok(value
        .lock()
        .map_err(|_| std::io::Error::other("goal fixture lock poisoned"))?)
}

impl Peer {
    fn write_permission(&self, task: &str, cwd: PathBuf) -> FixtureResult {
        let filename = if task == "join" {
            "joined.txt".to_owned()
        } else {
            format!("{task}.txt")
        };
        let permission_kind = if task == "right" && self.right_manual {
            "other"
        } else {
            "edit"
        };
        let overlap = task == "right" && self.right_overlap;
        let mut requests = vec![(
            "write-permission",
            "fixture-write",
            format!("Write {filename}"),
            "Allow this fixture write",
        )];
        if overlap {
            requests.push((
                "confirm-permission",
                "fixture-confirm",
                format!("Confirm {filename}"),
                "Allow second fixture write",
            ));
            self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":self.session,"update":{"sessionUpdate":"tool_call","toolCallId":"fixture-confirm",
                    "title":format!("Confirm {filename}"),"kind":"other","status":"in_progress",
                    "locations":[{"path":cwd.join(&filename)}]}}}))?;
        }
        lock(&self.state)?.permissions = requests
            .iter()
            .map(|request| request.0.to_owned())
            .collect();
        for (id, tool, title, allow) in requests {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission",
                "params":{"sessionId":self.session,"toolCall":{"toolCallId":tool,
                    "title":title,"kind":permission_kind,
                    "locations":[{"path":cwd.join(&filename)}],"rawInput":{"path":filename}},
                    "options":[{"optionId":"allow-once","name":allow,"kind":"allow_once"},
                        {"optionId":"deny-once","name":"Deny this fixture write","kind":"reject_once"}]}}))?;
        }
        Ok(())
    }
    fn send(&self, value: Value) -> FixtureResult {
        let _output = lock(&self.output)?;
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &value)?;
        writeln!(stdout)?;
        stdout.flush()?;
        Ok(())
    }
    fn respond(&self, id: &Value, result: Value) -> FixtureResult {
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
    fn update(&self, text: &str) -> FixtureResult {
        self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":self.session,"update":{"sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":text}}
        }}))
    }
    fn work(&self, prompt: Value, task: String, cwd: PathBuf) -> FixtureResult {
        if task == "join" {
            let left = std::fs::read_to_string(cwd.join("left.txt"))?;
            let right = std::fs::read_to_string(cwd.join("right.txt"))?;
            if left != "LEFT\n" || right != "RIGHT\n" {
                return Err("dependency material was not actually handed off".into());
            }
            std::fs::write(cwd.join("joined.txt"), format!("{left}{right}"))?;
        } else {
            let content = match task.as_str() {
                "left" => "LEFT\n",
                "right" => "RIGHT\n",
                _ => return Err("unknown fixture task".into()),
            };
            std::fs::write(cwd.join(format!("{task}.txt")), content)?;
            if task == "right" && self.right_overlap {
                if std::fs::read_to_string(cwd.join("right.txt"))? != "RIGHT\n" {
                    return Err(
                        "second authorized fixture operation did not confirm the file".into(),
                    );
                }
                self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":self.session,"update":{"sessionUpdate":"tool_call_update",
                        "toolCallId":"fixture-confirm","status":"completed"}}}))?;
            }
            if !self.omit_tool_completion {
                self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":self.session,"update":{"sessionUpdate":"tool_call_update",
                    "toolCallId":"fixture-write","status":"completed"}}}))?;
            }
            std::fs::write(
                self.control.join(format!("completed-{task}")),
                b"actual fixture write ended",
            )?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while !self.control.join("release").exists() {
                if self.cancelled.load(Ordering::Acquire) {
                    return self.respond(&prompt, json!({"stopReason":"cancelled"}));
                }
                if Instant::now() >= deadline {
                    return Err("fixture release deadline expired".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.update(&format!(
            "Fixture task {task}: file written; BRO must run its own checks."
        ))?;
        self.respond(&prompt, json!({"stopReason":"end_turn"}))
    }
    fn receive(self: &Arc<Self>, message: Value) -> FixtureResult {
        let id = &message["id"];
        match message["method"].as_str() {
            Some("providers/set") => {
                lock(&self.state)?.provider = Some(serde_json::from_value(message["params"].clone())?);
                self.respond(id, json!({}))?;
            }
            Some("providers/list") => {
                use agent_client_protocol::schema::v1::{ListProvidersResponse, ProviderInfo, ProviderCurrentConfig};
                let configured = lock(&self.state)?.provider.clone().ok_or("provider not configured")?;
                let current = ProviderCurrentConfig::new(configured.api_type.clone(), configured.base_url);
                let response = ListProvidersResponse::new(vec![ProviderInfo::new(configured.provider_id,
                    vec![configured.api_type], true, Some(current))]);
                self.respond(id, serde_json::to_value(response)?)?;
            }
            Some("initialize") => self.respond(id, json!({"protocolVersion":1,
                "agentCapabilities":{},"agentInfo":{"name":"goal-wire-fixture","version":"1"}}))?,
            Some("session/new") => {
                lock(&self.state)?.cwd = PathBuf::from(message["params"]["cwd"].as_str().ok_or("missing cwd")?);
                self.respond(id, json!({"sessionId":self.session}))?;
            }
            Some("session/prompt") => {
                let text = message["params"]["prompt"].as_array().ok_or("missing prompt")?
                    .iter().filter_map(|part| part["text"].as_str()).collect::<String>();
                let task = text.lines().find_map(|line| line.strip_prefix("Assigned task ")
                    .and_then(|line| line.strip_suffix(':'))).ok_or("missing assigned task")?.to_owned();
                if !matches!(task.as_str(), "left" | "right" | "join") {
                    return Err("unknown assigned task".into());
                }
                let cwd = {
                    let mut state = lock(&self.state)?;
                    state.pending = Some((id.clone(), task.clone()));
                    state.cwd.clone()
                };
                // This file is fixture instrumentation, outside delivered work.
                std::fs::write(self.control.join(format!("started-{task}")), cwd.to_string_lossy().as_bytes())?;
                self.update(&format!("Fixture {task} started; waiting for permission and release."))?;
                let filename = if task == "join" { "joined.txt".to_owned() } else { format!("{task}.txt") };
                self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":self.session,"update":{"sessionUpdate":"tool_call","toolCallId":"fixture-write",
                        "title":format!("Write {filename}"),"kind":"edit","status":"in_progress",
                        "locations":[{"path":cwd.join(&filename)}]}}}))?;
                if task == "right" && self.right_gate {
                    let peer = self.clone();
                    let prompt = id.clone();
                    std::thread::Builder::new().name("goal-fixture-permission-gate".into()).spawn(move || {
                        let result = (|| {
                            let deadline = Instant::now() + Duration::from_secs(15);
                            while !peer.control.join("permission-ready").exists() {
                                if peer.cancelled.load(Ordering::Acquire) {
                                    return peer.respond(&prompt, json!({"stopReason":"cancelled"}));
                                }
                                if Instant::now() >= deadline { return Err("permission gate deadline expired".into()); }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            peer.write_permission(&task, cwd)
                        })();
                        if let Err(error) = result { eprintln!("goal permission gate failed: {error}"); std::process::exit(1); }
                    })?;
                } else { self.write_permission(&task, cwd)?; }
            }
            Some("session/cancel") => {
                self.cancelled.store(true, Ordering::Release);
                if !id.is_null() { self.respond(id, json!({}))?; }
            }
            None if id == "write-permission" || id == "confirm-permission" => {
                let allowed = message["result"]["outcome"]["optionId"] == "allow-once";
                let pending = {
                    let mut state = lock(&self.state)?;
                    if !state.permissions.remove(id.as_str().ok_or("permission response has no identity")?) {
                        return Err("duplicate or unoffered permission response".into());
                    }
                    if allowed && !state.permissions.is_empty() {
                        std::fs::write(self.control.join("right-first-approved"), b"one actual response; another permission remains")?;
                        None
                    } else {
                        state.permissions.clear();
                        state.pending.take()
                    }
                };
                if let Some((prompt, task)) = pending {
                    if !allowed {
                        self.respond(&prompt, json!({"stopReason":"refusal"}))?;
                    } else {
                        let cwd = lock(&self.state)?.cwd.clone();
                        let peer = self.clone();
                        std::thread::Builder::new().name("goal-fixture-work".into()).spawn(move || {
                            if let Err(error) = peer.work(prompt, task, cwd) {
                                eprintln!("goal fixture work failed: {error}");
                                std::process::exit(1);
                            }
                        })?;
                    }
                }
            }
            _ => self.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unsupported fixture method"}}))?,
        }
        Ok(())
    }
}

fn main() -> FixtureResult {
    let control = PathBuf::from(
        std::env::var_os("BRO_GOAL_FIXTURE_CONTROL").ok_or("fixture control directory missing")?,
    );
    let capture = PathBuf::from(
        std::env::var_os("BRO_GOAL_FIXTURE_CAPTURE").ok_or("fixture capture directory missing")?,
    )
    .join(format!("{}.ndjson", std::process::id()));
    let peer = Arc::new(Peer {
        state: Mutex::new(State {
            provider: None,
            cwd: PathBuf::new(),
            pending: None,
            permissions: BTreeSet::new(),
        }),
        output: Mutex::new(()),
        cancelled: AtomicBool::new(false),
        control,
        session: format!("goal-fixture-{}", std::process::id()),
        right_manual: std::env::var("BRO_GOAL_FIXTURE_RIGHT_MANUAL")
            .is_ok_and(|value| value == "1"),
        right_gate: std::env::var("BRO_GOAL_FIXTURE_RIGHT_GATE").is_ok_and(|value| value == "1"),
        right_overlap: std::env::var("BRO_GOAL_FIXTURE_RIGHT_OVERLAP")
            .is_ok_and(|value| value == "1"),
        omit_tool_completion: std::env::var("BRO_GOAL_FIXTURE_OMIT_TOOL_COMPLETION")
            .is_ok_and(|value| value == "1"),
    });
    let mut capture = OpenOptions::new().create(true).append(true).open(capture)?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        writeln!(capture, "{line}")?;
        capture.flush()?;
        peer.receive(serde_json::from_str(&line)?)?;
    }
    peer.cancelled.store(true, Ordering::Release);
    Ok(())
}
