//! Deterministic ACP peer for the Unix PTY acceptance suite.

#[cfg(unix)]
mod fixture {
    use std::collections::BTreeSet;
    use std::error::Error;
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    type FixtureResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    struct State {
        pending: Option<Value>,
        permissions: BTreeSet<String>,
        prompt_count: usize,
        session_count: usize,
        session: String,
    }

    struct Peer {
        scenario: String,
        state: Mutex<State>,
        output: Mutex<()>,
    }

    fn lock<T>(value: &Mutex<T>) -> FixtureResult<std::sync::MutexGuard<'_, T>> {
        Ok(value
            .lock()
            .map_err(|_| std::io::Error::other("fixture lock poisoned"))?)
    }

    fn settings(current: &str) -> Value {
        json!([{
            "id": "a12-setting", "name": "A12 setting",
            "description": "Fixture setting for confirmation coverage",
            "type": "select", "currentValue": current,
            "options": [
                {"value": "a12-old", "name": "A12 old"},
                {"value": "a12-confirmed", "name": "A12 confirmed"}
            ]
        }])
    }

    impl Peer {
        fn send(&self, value: Value) -> FixtureResult {
            let _guard = lock(&self.output)?;
            let mut output = std::io::stdout().lock();
            serde_json::to_writer(&mut output, &value)?;
            writeln!(output)?;
            output.flush()?;
            Ok(())
        }

        fn respond(&self, id: &Value, result: Value) -> FixtureResult {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
        }

        fn update(&self, text: &str, session: Option<&str>) -> FixtureResult {
            let session = match session {
                Some(session) => session.to_owned(),
                None => lock(&self.state)?.session.clone(),
            };
            self.send(json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": session, "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }}
            }))
        }

        fn finish(&self, id: &Value, reason: &str) -> FixtureResult {
            self.respond(id, json!({"stopReason": reason}))
        }

        fn settle(&self, reason: &str, text: &str) -> FixtureResult {
            let pending = lock(&self.state)?.pending.take();
            if let Some(id) = pending {
                self.update(text, None)?;
                self.finish(&id, reason)?;
            }
            Ok(())
        }

        fn permissions(&self) -> FixtureResult {
            lock(&self.state)?.permissions = ["permission-1".into(), "permission-2".into()].into();
            for number in 1..=2 {
                self.send(json!({
                    "jsonrpc": "2.0", "id": format!("permission-{number}"),
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": "pty-native",
                        "toolCall": {"toolCallId": format!("tool-{number}"), "title": format!("fixture permission {number}")},
                        "options": [
                            {"optionId": format!("allow-{number}"), "name": format!("Allow fixture {number}"), "kind": "allow_once"},
                            {"optionId": format!("deny-{number}"), "name": format!("Deny fixture {number}"), "kind": "reject_once"}
                        ]
                    }
                }))?;
            }
            Ok(())
        }

        fn controls(&self, server: UnixListener) -> FixtureResult {
            for connection in server.incoming() {
                let mut connection = connection?;
                let mut command = String::new();
                BufReader::new(&connection)
                    .take(128)
                    .read_line(&mut command)?;
                match command.trim() {
                    "release" => self.settle("end_turn", "FXRL")?,
                    "refusal" => self.settle("refusal", "FXST")?,
                    "permission" => self.permissions()?,
                    "backlog" => {
                        let pending = lock(&self.state)?.pending.take();
                        if let Some(id) = pending {
                            for number in 0..90 {
                                self.update(&format!("BG{number:03}\n"), None)?;
                            }
                            self.finish(&id, "end_turn")?;
                        }
                    }
                    _ => {}
                }
                connection.write_all(b"ok\n")?;
                if command.trim() == "disconnect" {
                    std::process::exit(0);
                }
            }
            Ok(())
        }

        // ACP session and permission methods: https://agentclientprotocol.com/protocol/overview
        fn receive(&self, message: Value) -> FixtureResult {
            let id = &message["id"];
            match message["method"].as_str() {
                Some("initialize") => {
                    let capabilities = if matches!(self.scenario.as_str(), "settings-confirmed" | "settings-failure" | "session-lifecycle") {
                        json!({"loadSession": true, "sessionCapabilities": {"list": {}, "resume": {}}})
                    } else {
                        json!({})
                    };
                    self.respond(id, json!({"protocolVersion": 1, "agentCapabilities": capabilities, "agentInfo": {"name": "pty-minimal", "version": "1"}}))?;
                }
                Some("session/new") => {
                    if self.scenario == "hanging-session" {
                        return Ok(());
                    }
                    if self.scenario == "fresh-sessions" {
                        let mut state = lock(&self.state)?;
                        state.session_count = state.session_count.checked_add(1).ok_or_else(|| {
                            std::io::Error::other("fixture session count overflow")
                        })?;
                        state.session = format!("pty-fresh-{}-{}", std::process::id(), state.session_count);
                    }
                    if matches!(self.scenario.as_str(), "settings-confirmed" | "settings-failure") {
                        self.respond(id, json!({"sessionId": "pty-native", "configOptions": settings("a12-old"), "_meta": {"agentSessionId": "agent-a12-settings"}}))?;
                        self.update("FXSET", None)?;
                    } else {
                        let session = lock(&self.state)?.session.clone();
                        self.respond(id, json!({"sessionId": session}))?;
                    }
                    let ready = if self.scenario == "fresh-sessions" {
                        format!("FXRD {}", lock(&self.state)?.session)
                    } else {
                        "FXRD".into()
                    };
                    self.update(&ready, None)?;
                }
                Some("session/load") => {
                    if self.scenario == "session-lifecycle" {
                        let session = message["params"]["sessionId"].as_str().unwrap_or("native-a12-load");
                        self.update("FXLOAD", Some(session))?;
                        self.respond(id, json!({"configOptions": settings("a12-old"), "_meta": {"agentSessionId": "agent-a12-load"}}))?;
                    } else {
                        self.respond(id, json!({"_meta": {"loadedBy": "pty-fixture"}}))?;
                    }
                }
                Some("session/resume") => self.respond(id, json!({"configOptions": settings("a12-old"), "_meta": {"agentSessionId": "agent-a12-resume"}}))?,
                Some("session/set_config_option") => match self.scenario.as_str() {
                    "settings-confirmed" => self.respond(id, json!({"configOptions": settings("a12-confirmed")}))?,
                    "settings-failure" => self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "FXSF"}}))?,
                    _ => self.respond(id, json!({"configOptions": []}))?,
                },
                Some("session/prompt") => {
                    let count = {
                        let mut state = lock(&self.state)?;
                        state.prompt_count += 1;
                        state.prompt_count
                    };
                    let text = message.to_string();
                    if text.contains("background-isolation-fixture") {
                        self.update("BACKGROUND_PRIVATE_OUTPUT", None)?;
                        self.finish(id, "end_turn")?;
                    } else {
                        let marker = if self.scenario == "fresh-sessions" && text.contains("wait-for-timeout") {
                            Some("FXWAIT".to_owned())
                        } else {
                            match self.scenario.as_str() {
                                "delayed-cancel" => Some("FXCN".into()),
                                "delayed-normal" => Some(format!("FXW{count}")),
                                "delayed-refusal" => Some("FXRF".into()),
                                "overlapping-permissions" => Some(String::new()),
                                "detached-backlog" => Some("FXBG".into()),
                                _ => None,
                            }
                        };
                        if let Some(marker) = marker {
                            lock(&self.state)?.pending = Some(id.clone());
                            if self.scenario == "overlapping-permissions" {
                                self.permissions()?;
                            } else {
                                self.update(&marker, None)?;
                            }
                        } else {
                            self.update(&format!("FXRP{count}"), None)?;
                            self.finish(id, "end_turn")?;
                        }
                    }
                }
                Some("session/cancel") => {
                    if !id.is_null() {
                        self.respond(id, json!({}))?;
                    }
                    self.settle("cancelled", "FXCS")?;
                }
                _ => {
                    let pending = {
                        let mut state = lock(&self.state)?;
                        if id.as_str().is_some_and(|id| state.permissions.remove(id)) && state.permissions.is_empty() {
                            state.pending.take()
                        } else {
                            None
                        }
                    };
                    if let Some(id) = pending {
                        self.finish(&id, "end_turn")?;
                    }
                }
            }
            Ok(())
        }
    }

    pub fn run() -> FixtureResult {
        let peer = Arc::new(Peer {
            scenario: std::env::var("BITROUTER_PTY_SCENARIO").unwrap_or_else(|_| "minimal".into()),
            state: Mutex::new(State {
                pending: None,
                permissions: BTreeSet::new(),
                prompt_count: 0,
                session_count: 0,
                session: "pty-native".into(),
            }),
            output: Mutex::new(()),
        });
        if let Some(path) = std::env::var_os("BITROUTER_PTY_CONTROL") {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let server = UnixListener::bind(path)?;
            let controls = peer.clone();
            std::thread::Builder::new()
                .name("pty-fixture-controls".into())
                .spawn(move || {
                    if let Err(error) = controls.controls(server) {
                        eprintln!("PTY fixture control failed: {error}");
                        std::process::exit(1);
                    }
                })?;
        }
        let mut capture = OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::var_os("BITROUTER_PTY_CAPTURE").ok_or("capture path missing")?)?;
        let mut input = std::io::stdin().lock();
        let mut line = String::new();
        while input.read_line(&mut line)? != 0 {
            capture.write_all(line.as_bytes())?;
            capture.flush()?;
            peer.receive(serde_json::from_str(&line)?)?;
            line.clear();
        }
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--editor")) {
        return editor_fixture();
    }
    fixture::run()
}

#[cfg(unix)]
fn editor_fixture() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let draft = std::env::args_os()
        .nth(2)
        .ok_or("editor draft path missing")?;
    let capture =
        std::env::var_os("BITROUTER_PTY_EDITOR_CAPTURE").ok_or("editor capture path missing")?;
    std::fs::write(&capture, std::fs::read(&draft)?)?;
    if let Some(path) = std::env::var_os("BITROUTER_PTY_EDITOR_PID") {
        std::fs::write(path, std::process::id().to_string())?;
    }
    if let Some(path) = std::env::var_os("BITROUTER_PTY_EDITOR_HOLD") {
        while !std::path::Path::new(&path).exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let replacement = std::env::var_os("BITROUTER_PTY_EDITOR_CONTENT")
        .ok_or("editor replacement path missing")?;
    std::fs::write(draft, std::fs::read(replacement)?)?;
    if std::env::var_os("BITROUTER_PTY_EDITOR_FAIL").is_some() {
        return Err("fixture editor failed after writing a recoverable file".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {}
