//! Actual Goals CLI/terminal boundary; no provider judgments are simulated here.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bitrouter::daemon::{DaemonCommand, DaemonResponse, send_command};
use bitrouter_orchestrator::goal::{GoalCommand, GoalReply, GoalSnapshot, budget::Limits};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::process::Command;

#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;
#[path = "support/goal_provider.rs"]
mod provider;

struct Terminal {
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    ui_pid: tempfile::NamedTempFile,
    screen: vt100::Parser,
    raw: Vec<u8>,
    initial_modes: nix::libc::termios,
}

fn terminal_modes(modes: nix::sys::termios::Termios) -> nix::libc::termios {
    let modes: nix::libc::termios = modes.into();
    #[cfg(target_os = "macos")]
    let modes = {
        let mut modes = modes;
        // XNU sets PENDIN when restoring ICANON, even for an empty input queue.
        // It is transient driver state. Compare every actual setting unchanged.
        // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c
        modes.c_lflag &= !nix::libc::PENDIN;
        modes
    };
    modes
}

impl Terminal {
    fn open(home: &Path, code: bool) -> Result<Self> {
        Self::open_with_environment(home, code, &[])
    }

    fn open_with_environment(
        home: &Path,
        code: bool,
        environment: &[(&str, &std::ffi::OsStr)],
    ) -> Result<Self> {
        let args = if code {
            vec![
                std::ffi::OsString::from("code"),
                std::ffi::OsString::from("--config"),
                home.join("bitrouter.yaml").into_os_string(),
            ]
        } else {
            Vec::new()
        };
        Self::open_args(home, &args, environment)
    }

    fn open_args(
        home: &Path,
        args: &[std::ffi::OsString],
        environment: &[(&str, &std::ffi::OsStr)],
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let initial_modes = terminal_modes(
            pair.master
                .get_termios()
                .ok_or_else(|| anyhow::anyhow!("PTY terminal modes unavailable"))?,
        );
        let ui_pid = tempfile::NamedTempFile::new_in(home)?;
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("\"$@\" <&0 &\npid=$!\nprintf '%s\\n' \"$pid\" > \"$BRO_GOAL_PTY_UI_PID\"\nwait \"$pid\"\nstatus=$?\nprintf '\\nGOAL_UI_EXIT:%s\\n' \"$status\"\nif [ \"$status\" -ne 0 ]; then exit \"$status\"; fi\nprintf 'GOAL_SHELL_READY\\n'\nIFS= read -r input\nprintf 'GOAL_SHELL_OK<%s>\\n' \"$input\"");
        command.arg("goal-shell");
        command.arg(env!("CARGO_BIN_EXE_bro"));
        command.cwd(home);
        command.env("HOME", home);
        command.env("BITROUTER_HOME", home);
        command.env("XDG_DATA_HOME", home.join("data"));
        command.env("BRO_GOAL_PTY_UI_PID", ui_pid.path());
        for (key, value) in environment {
            command.env(key, value);
        }
        for arg in args {
            command.arg(arg);
        }
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            writer,
            output,
            child,
            master: pair.master,
            ui_pid,
            screen: vt100::Parser::new(24, 100, 2000),
            raw: Vec::new(),
            initial_modes,
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn ui_pid(&self) -> Result<u32> {
        std::fs::read_to_string(self.ui_pid.path())?
            .trim()
            .parse()
            .context("reading the owned BRO child PID")
    }

    fn receive(&mut self) {
        if let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(50)) {
            self.screen.process(&bytes);
            self.raw.extend(bytes);
        }
    }

    fn wait(&mut self, text: &str) -> Result<()> {
        self.wait_until(text, Instant::now() + Duration::from_secs(15))
    }

    fn wait_until(&mut self, text: &str, deadline: Instant) -> Result<()> {
        loop {
            self.receive();
            let begin = self
                .raw
                .windows(8)
                .rposition(|bytes| bytes == b"\x1b[?2026h");
            let end = self
                .raw
                .windows(8)
                .rposition(|bytes| bytes == b"\x1b[?2026l");
            if begin.is_none_or(|begin| end.is_some_and(|end| end > begin))
                && self.screen.screen().contents().contains(text)
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "missing {text:?}; screen: {:?}; output: {:?}",
                self.screen.screen().contents(),
                String::from_utf8_lossy(&self.raw[self.raw.len().saturating_sub(2048)..])
            );
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.screen.set_size(rows, cols);
        Ok(())
    }

    fn resize_and_wait(&mut self, rows: u16, cols: u16, text: &str) -> Result<()> {
        self.resize(rows, cols)?;
        self.redraw_and_wait(text)
    }

    // On a stable screen, a complete repaint of this text acknowledges Ctrl-L
    // and every preceding key. An arbitrary frame can be an older queued draw.
    fn redraw_and_wait(&mut self, text: &str) -> Result<()> {
        while let Ok(bytes) = self.output.try_recv() {
            self.screen.process(&bytes);
            self.raw.extend(bytes);
        }
        let previous = self.raw.len();
        self.send(b"\x0c")?;
        self.wait_fresh_text(previous, text)
    }

    fn wait_fresh_text(&mut self, previous: usize, text: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.receive();
            let begin = self.raw[previous..]
                .windows(8)
                .rposition(|bytes| bytes == b"\x1b[?2026h");
            let end = self.raw[previous..]
                .windows(8)
                .rposition(|bytes| bytes == b"\x1b[?2026l");
            if begin.is_some_and(|begin| end.is_some_and(|end| end > begin))
                && self.raw[previous..]
                    .windows(text.len())
                    .any(|bytes| bytes == text.as_bytes())
                && self.screen.screen().contents().contains(text)
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "explicit redraw did not repaint {text:?} in a complete frame"
            );
        }
        Ok(())
    }

    fn wait_alternate(&mut self, expected: bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.receive();
            if self.screen.screen().alternate_screen() == expected {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "inspector transition did not finish"
            );
        }
    }

    async fn suspend_resize_resume(&mut self) -> Result<()> {
        let pid = self.ui_pid()?;
        let before = self.raw.len();
        ensure!(
            Command::new("kill")
                .args(["-TSTP", &pid.to_string()])
                .status()
                .await?
                .success()
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = Command::new("ps")
                .args(["-o", "state=", "-p", &pid.to_string()])
                .output()
                .await?;
            self.receive();
            if String::from_utf8_lossy(&state.stdout).contains('T') {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Goal UI did not suspend"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        while !self.raw[before..]
            .windows(8)
            .any(|bytes| bytes == b"\x1b[?2004l")
        {
            self.receive();
            ensure!(
                tokio::time::Instant::now() < deadline,
                "suspension did not restore terminal keys"
            );
        }
        ensure!(!self.screen.screen().alternate_screen());
        self.assert_modes_restored()?;
        self.resize(20, 60)?;
        let resumed = self.raw.len();
        ensure!(
            Command::new("kill")
                .args(["-CONT", &pid.to_string()])
                .status()
                .await?
                .success()
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !self.raw[resumed..]
            .windows(8)
            .any(|bytes| bytes == b"\x1b[?2004h")
        {
            self.receive();
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Goal UI did not reacquire terminal keys"
            );
        }
        Ok(())
    }

    fn detach(mut self) -> Result<()> {
        self.send(b"\x11")?;
        self.finish()
    }

    async fn signal_exit(self, signal: &str) -> Result<()> {
        let pid = self.ui_pid()?;
        ensure!(
            Command::new("kill")
                .args([signal, &pid.to_string()])
                .status()
                .await?
                .success(),
            "could not signal owned Goal UI"
        );
        self.finish()
    }

    fn finish(mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        self.wait_until("GOAL_UI_EXIT:0", deadline)?;
        self.assert_modes_restored()?;
        self.wait_until("GOAL_SHELL_READY", deadline)?;
        self.send("  shell input 中文  \n".as_bytes())
            .context("sending input to the post-Goal shell")?;
        self.wait_until("GOAL_SHELL_OK<  shell input 中文  >", deadline)?;
        loop {
            self.receive();
            if let Some(status) = self.child.try_wait()? {
                ensure!(status.success(), "shell failed after Goal UI: {status}");
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "shell did not finish after Goal UI"
            );
        }
        loop {
            match self.output.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    self.screen.process(&bytes);
                    self.raw.extend(bytes);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => ensure!(
                    Instant::now() < deadline,
                    "Goal UI output reader did not finish"
                ),
            }
        }
        ensure!(!self.screen.screen().alternate_screen());
        ensure!(self.raw.windows(8).any(|bytes| bytes == b"\x1b[?2004l"));
        self.assert_modes_restored()?;
        Ok(())
    }

    fn assert_modes_restored(&self) -> Result<()> {
        let actual = self
            .master
            .get_termios()
            .ok_or_else(|| anyhow::anyhow!("PTY terminal modes unavailable after cleanup"))?;
        let actual = terminal_modes(actual);
        ensure!(
            actual == self.initial_modes,
            "Goal UI did not restore terminal modes: {actual:?}; expected {:?}",
            self.initial_modes
        );
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

async fn request(socket: &Path, command: GoalCommand) -> Result<GoalReply> {
    let reply = send_command(
        socket,
        &DaemonCommand::Goals {
            protocol: bitrouter_orchestrator::goal::PROTOCOL_VERSION,
            command,
        },
    )
    .await?;
    let DaemonResponse::Goals { response, .. } = reply else {
        anyhow::bail!("unexpected Goal reply: {reply:?}");
    };
    response.map_err(|error| anyhow::anyhow!("Goal request rejected: {error:?}"))
}

async fn directory(socket: &Path) -> Result<usize> {
    let GoalReply::Directory { page } = request(
        socket,
        GoalCommand::List {
            after: 0,
            cutoff: None,
            limit: 16,
        },
    )
    .await?
    else {
        anyhow::bail!("expected Goal directory");
    };
    Ok(page.goals.len())
}

async fn head(socket: &Path, goal_id: &str) -> Result<Box<GoalSnapshot>> {
    let GoalReply::Snapshot { goal } = request(
        socket,
        GoalCommand::Get {
            goal_id: goal_id.into(),
        },
    )
    .await?
    else {
        anyhow::bail!("expected Goal snapshot");
    };
    Ok(goal)
}

async fn wait_goal(
    socket: &Path,
    goal_id: &str,
    ready: impl Fn(&GoalSnapshot) -> bool,
) -> Result<Box<GoalSnapshot>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let goal = head(socket, goal_id).await?;
        if ready(&goal) {
            return Ok(goal);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Goal did not reach expected state: {:?}",
            goal.workflow
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn history(
    socket: &Path,
    goal_id: &str,
    cutoff: u64,
) -> Result<Vec<bitrouter_orchestrator::goal::GoalOperation>> {
    let mut after = 0;
    let mut operations = Vec::new();
    loop {
        let GoalReply::History { page } = request(
            socket,
            GoalCommand::History {
                goal_id: goal_id.into(),
                after,
                cutoff: Some(cutoff),
                limit: bitrouter_orchestrator::goal::MAX_PAGE_ITEMS,
            },
        )
        .await?
        else {
            anyhow::bail!("Goal history omitted its page");
        };
        ensure!(page.cutoff == cutoff, "history cutoff changed");
        operations.extend(page.operations);
        let Some(next) = page.next_after else {
            return Ok(operations);
        };
        ensure!(next > after && next <= cutoff, "history did not advance");
        after = next;
    }
}

fn running(goal: &GoalSnapshot) -> usize {
    goal.workflow
        .tasks
        .iter()
        .filter(|task| {
            task.executions.last().is_some_and(|execution| {
                matches!(
                    execution.observation,
                    bitrouter_orchestrator::goal::workflow::Observation::Running { .. }
                )
            })
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_goal_enter_permission_readonly_workflow_and_detach_use_actual_execution()
-> Result<()> {
    use bitrouter_orchestrator::goal::decision::PermissionState;
    use bitrouter_orchestrator::goal::{OperationKind, RunIntent};
    use wiremock::{
        Mock, MockServer,
        matchers::{method, path},
    };
    let binary =
        example_binary::build("bitrouter", "goal_acp_fixture").map_err(anyhow::Error::msg)?;
    let home = tempfile::tempdir()?;
    let workspace = home.path().join("original");
    let controls = home.path().join("controls");
    let captures = home.path().join("captures");
    for directory in [&workspace, &controls, &captures] {
        std::fs::create_dir_all(directory)?;
    }
    std::fs::write(workspace.join("seed.txt"), "original input\n")?;
    for args in [
        vec!["init", "-q"],
        vec!["add", "seed.txt"],
        vec![
            "-c",
            "user.name=Goal Fixture",
            "-c",
            "user.email=goal-fixture@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "fixture baseline",
        ],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&workspace)
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "fixture Git setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(provider::model)
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(provider::typed)
        .mount(&backend)
        .await;
    let config_path = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&serde_json::json!({
            "inherit_defaults":false,"registry":{"enabled":false},
            "server":{"listen":"127.0.0.1:0","skip_auth":true},
            "database":{"url":format!("sqlite://{}?mode=rwc",home.path().join("goals.db").display())},
            "chat":{"model":"goal-model"},
            "providers":{"fixture":{"api_base":format!("{}/v1",backend.uri()),
                "api_key":"goal-fixture-key","api_protocol":[{"*":"chat_completions"}],
                "models":[{"id":"test-model"}]}},
            "models":{"goal-model":{"endpoints":[{"provider":"fixture","service_id":"test-model"}]}},
            "decision_model":{"model":"fixture-input","base_url":backend.uri(),"api_key_env":"BRO_GOAL_FIXTURE_DECISION_KEY"},
            "agents":{"goal-peer":{"name":"goal-peer","transport":{"type":"stdio","command":binary,"args":[],
                "env":{"BRO_GOAL_FIXTURE_CONTROL":controls,"BRO_GOAL_FIXTURE_CAPTURE":captures,"BRO_GOAL_FIXTURE_RIGHT_MANUAL":"1","BRO_GOAL_FIXTURE_RIGHT_GATE":"1","BRO_GOAL_FIXTURE_RIGHT_OVERLAP":"1"}}}}
        }))?,
    )?;
    let source = bitrouter::paths::ConfigSource::File(config_path.clone());
    let config = bitrouter::paths::load_config(&source).await?;
    let socket = bitrouter::daemon::socket_path_for(&source, &config);
    let mut server = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(["serve", "--config"])
        .arg(&config_path)
        .env("HOME", home.path())
        .env("BITROUTER_HOME", home.path())
        .env("BRO_GOAL_FIXTURE_DECISION_KEY", "fixture-decision-key")
        .current_dir(home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let result = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while directory(&socket).await.is_err() {
            ensure!(server.try_wait()?.is_none(), "fixture daemon exited");
            ensure!(tokio::time::Instant::now() < deadline, "fixture daemon not ready");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut terminal = Terminal::open(home.path(),true)?;
        terminal.wait("No matching Goals")?;
        ensure!(backend.received_requests().await.is_some_and(|requests| requests.is_empty()));
        let GoalReply::Accepted { operation } = request(&socket,GoalCommand::Create {
            operation_id:"pty-active-goal".into(), title:"PTY active parallel Goal".into(),
            workspace:workspace.clone(),objective:"Deliver LEFT and RIGHT files in parallel, then join their actual contents. BRO must verify all three files. No human acceptance is required.".into(),
            budget_limits:Default::default(),
        }).await? else { anyhow::bail!("Goal creation omitted receipt"); };
        let id = operation.goal_id;
        let active = wait_goal(&socket,&id,|goal| running(goal) == 2
            && controls.join("started-right").exists()
            && controls.join("completed-left").exists()).await?;
        ensure!(!active.workflow.decisions.iter().any(|decision| decision.state == PermissionState::Pending));
        ensure!(!controls.join("completed-right").exists());
        terminal.send(b"r")?;
        terminal.wait("PTY active parallel Goal")?;
        terminal.send(b"\r")?;
        terminal.wait("scope: this Goal")?;
        let query = "  What is currently active? 中文 👩‍💻\nDo not change anything.  ";
        terminal.send(format!("\x1b[200~{query}\x1b[201~").as_bytes())?;
        terminal.wait("Do not change anything.")?;
        ensure!(head(&socket,&id).await?.inputs.latest.is_none(), "paste submitted input");
        terminal.send(b"\r")?;
        let answered = wait_goal(&socket,&id,|goal| goal.inputs.latest.as_ref()
            .is_some_and(|input| input.answer.is_some())).await?;
        ensure!(running(&answered) == 2, "Enter waited for workers to settle");
        ensure!(answered.requirements_revision == 1 && answered.run_intent == RunIntent::Run);
        let input = answered.inputs.latest.as_ref().ok_or_else(|| anyhow::anyhow!("input result missing"))?;
        ensure!(!input.requirements_saved && input.control_saved.is_none());
        let input_history = history(&socket,&id,answered.revision).await?;
        ensure!(input_history.iter().any(|operation| matches!(&operation.kind,
            OperationKind::Input { text,task_reference:None } if text == query)));
        terminal.wait("Fixture explanation")?;
        terminal.send(b"\x1b[A\r")?;
        let replayed = wait_goal(&socket,&id,|goal| goal.inputs.latest.as_ref().is_some_and(|latest|
            latest.answer.is_some() && latest.input_id != input.input_id)).await?;
        let replay_history = history(&socket,&id,replayed.revision).await?;
        ensure!(replay_history.iter().filter(|operation| matches!(&operation.kind,
            OperationKind::Input { text,task_reference:None } if text == query)).count() == 2,
            "history did not submit the same original bytes exactly twice");
        ensure!(running(&replayed) == 2 && replayed.requirements_revision == 1 && replayed.run_intent == RunIntent::Run);
        let ambiguous = "/task left FIXTURE_NEW_LEFT_RULE: BAD_RESOURCE_CHANGE_FIXTURE select an unavailable server";
        terminal.send(format!("\x1b[200~{ambiguous}\x1b[201~").as_bytes())?;
        terminal.send(b"\r")?;
        let questioned = wait_goal(&socket,&id,|goal|goal.inputs.latest.as_ref().is_some_and(|result|result.clarification.is_some())).await?;
        let question_id = questioned.inputs.latest.as_ref().ok_or_else(||anyhow::anyhow!("missing actual question"))?.input_id.clone();
        ensure!(questioned.inputs.completion_waits_for_input());
        terminal.wait("Resource changes were not accepted")?;
        let withdrawal = format!("/input {question_id} /withdraw");
        terminal.send(format!("\x1b[200~{withdrawal}\x1b[201~").as_bytes())?;
        terminal.send(b"\r")?;
        let withdrawn = wait_goal(&socket,&id,|goal|goal.inputs.unresolved_inputs().is_empty()
            && goal.inputs.latest.as_ref().is_some_and(|result|result.resolutions.iter().any(|resolution|resolution.input_id == question_id))).await?;
        ensure!(withdrawn.requirements_revision == questioned.requirements_revision && withdrawn.run_intent == RunIntent::Run && running(&withdrawn) == 2);
        ensure!(withdrawn.control_source_revision == questioned.control_source_revision && withdrawn.budget.limits == questioned.budget.limits);
        let withdrawal_history = history(&socket,&id,withdrawn.revision).await?;
        ensure!(withdrawal_history.iter().any(|operation|matches!(&operation.kind,OperationKind::InputWithdrawn {input_id,..} if input_id == &question_id)));
        ensure!(!withdrawal_history.iter().any(|operation|matches!(&operation.kind,OperationKind::Input {text,..} if text == &withdrawal)));
        let captured = backend.received_requests().await.ok_or_else(||anyhow::anyhow!("missing backend capture"))?;
        ensure!(!captured.iter().any(|request|String::from_utf8_lossy(&request.body).contains(&withdrawal)), "explicit withdrawal was forwarded to a model");
        terminal.wait("disposition saved: Withdrawn")?;
        terminal.send(b"UNSENT_ACTIVE_DRAFT")?;
        terminal.wait("UNSENT_ACTIVE_DRAFT")?;
        ensure!(!terminal.raw.windows(8).any(|bytes| bytes == b"\x1b[?1049h"), "middle entered alternate screen");
        let before = head(&socket,&id).await?;
        terminal.send(b"\x1b[1;5C")?;
        terminal.wait("Workflow · read only")?;
        terminal.send(b"\r")?;
        terminal.wait("Task / evidence · read only")?;
        terminal.send(b"edit branch\r\x01\x10\x18\x12\x02")?;
        terminal.wait("\"permission_id\"")?;
        terminal.send(b"\x06")?;
        terminal.wait_alternate(true)?;
        std::fs::write(controls.join("permission-ready"), b"permission arrives while reading")?;
        let pending = wait_goal(&socket,&id,|goal| goal.workflow.decisions.iter()
            .filter(|decision| decision.state == PermissionState::Pending).count() == 2).await?;
        let pending_decisions = pending.workflow.decisions.iter().filter(|decision| decision.state == PermissionState::Pending).cloned().collect::<Vec<_>>();
        let decision = pending_decisions.first().ok_or_else(|| anyhow::anyhow!("actual permission missing"))?.clone();
        let second = pending_decisions.get(1).ok_or_else(|| anyhow::anyhow!("second actual permission missing"))?.clone();
        ensure!(decision.id != second.id && decision.permission_id != second.permission_id && decision.request != second.request);
        ensure!(decision.execution_id == second.execution_id && decision.run_id == second.run_id
            && decision.requirements_revision == second.requirements_revision && decision.task_revision == second.task_revision);
        terminal.send(b"1\r\x01")?;
        ensure!(head(&socket,&id).await?.workflow.decisions.iter().filter(|current| current.state == PermissionState::Pending).count() == 2);
        ensure!(!controls.join("completed-right").exists(), "inspector keys answered a new permission");
        terminal.send(b"\x1b")?;
        terminal.wait_alternate(false)?;
        terminal.send(b"\x1b")?;
        terminal.wait("Write and verify left.txt")?;
        terminal.send(b"\x1b")?;
        terminal.wait("Workflow · read only")?;
        terminal.send(b"\x1b")?;
        terminal.wait("UNSENT_ACTIVE_DRAFT")?;
        let after = head(&socket,&id).await?;
        ensure!(after.inputs == before.inputs && after.requirements_revision == before.requirements_revision);
        ensure!(after.budget.limits == before.budget.limits && after.control_source_revision == before.control_source_revision,
            "read-only keys changed budget or control source");
        ensure!(after.run_intent == RunIntent::Run && running(&after) == 2);
        ensure!(after.workflow.tasks.iter().zip(&before.workflow.tasks).all(|(actual,original)| {
            actual.spec == original.spec && actual.run_intent == original.run_intent
                && actual.requirements_revision == original.requirements_revision
                && actual.executions.iter().map(|execution| &execution.id).collect::<Vec<_>>()
                    == original.executions.iter().map(|execution| &execution.id).collect::<Vec<_>>()
        }), "read-only keys changed branch intent or execution");
        ensure!(after.workflow.decisions.iter().any(|current| current.id == decision.id && current.state == PermissionState::Pending));
        ensure!(!controls.join("completed-right").exists(), "read-only keys granted permission");
        terminal.send(b"\x1b[1;5D")?;
        terminal.wait("needs you")?;
        ensure!(!terminal.screen.screen().alternate_screen());
        terminal.send(b"\r")?;
        terminal.wait("scope: this Goal")?;
        terminal.wait("UNSENT_ACTIVE_DRAFT")?;
        terminal.send(b"\x01")?;
        terminal.wait("Permission for task right")?;
        terminal.wait("Allow this fixture write")?;
        terminal.wait("Requested operation (harness reported)")?;
        terminal.send(b"\r")?;
        ensure!(head(&socket,&id).await?.workflow.decisions.iter().any(|current| current.id == decision.id && current.state == PermissionState::Pending));
        ensure!(!controls.join("completed-right").exists(), "permission card defaulted to allow");
        terminal.resize_and_wait(12,40,"Allow this fixture write")?;
        terminal.wait("Allow this fixture write")?;
        terminal.send(b"\x1b[B\t")?;
        terminal.wait("Allow second fixture write")?;
        terminal.send(b"\r")?;
        terminal.redraw_and_wait("Allow second fixture write")?;
        ensure!(head(&socket,&id).await?.workflow.decisions.iter().filter(|current| current.state == PermissionState::Pending).count() == 2,
            "Tab carried consent into another request");
        terminal.send(b"\t")?;
        terminal.wait("Allow this fixture write")?;
        terminal.resize_and_wait(10,30,"Resize to at least")?;
        terminal.wait("Resize to at least")?;
        terminal.send(b"\x1b[B\r1\r\x10\x18\x12")?;
        terminal.redraw_and_wait("Resize to at least")?;
        let small = head(&socket,&id).await?;
        ensure!(small.run_intent == RunIntent::Run && small.control_source_revision == after.control_source_revision);
        ensure!(small.workflow.decisions.iter().filter(|current| current.state == PermissionState::Pending).count() == 2);
        ensure!(!controls.join("completed-right").exists(), "small viewport granted permission");
        terminal.resize_and_wait(12,40,"Allow this fixture write")?;
        terminal.wait("Allow this fixture write")?;
        terminal.send(b"\r")?;
        terminal.redraw_and_wait("Allow this fixture write")?;
        ensure!(head(&socket,&id).await?.workflow.decisions.iter().any(|current| current.id == decision.id && current.state == PermissionState::Pending));
        terminal.resize_and_wait(24,100,"Allow this fixture write")?;
        terminal.send(b"\x1b[1;5D")?;
        terminal.wait("needs you")?;
        terminal.send(b"\r")?;
        terminal.wait("UNSENT_ACTIVE_DRAFT")?;
        terminal.send(b"\x01")?;
        terminal.wait("Allow this fixture write")?;
        terminal.resize_and_wait(12,40,"Allow this fixture write")?;
        terminal.wait("Allow this fixture write")?;
        terminal.send(b"\r")?;
        ensure!(!controls.join("completed-right").exists(), "reopened card reused a selection");
        terminal.send(b"\x1b[B\r")?;
        tokio::time::timeout(Duration::from_secs(15),async {
            loop {
                let current = head(&socket,&id).await?;
                let committed = history(&socket,&id,current.revision).await?;
                if controls.join("right-first-approved").exists() && committed.iter().any(|operation| {
                    matches!(&operation.kind,OperationKind::Workflow { change } if matches!(change.as_ref(),
                        bitrouter_orchestrator::goal::workflow::Change::Decision {
                            event:bitrouter_orchestrator::goal::decision::Event::Result {
                                decision_id,submitted:true,..
                            }
                        } if decision_id == &decision.id))
                }) { break Ok::<_,anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        let one_answered = head(&socket,&id).await?;
        ensure!(one_answered.workflow.decisions.iter().any(|current| current.id == second.id && current.state == PermissionState::Pending));
        ensure!(!controls.join("completed-right").exists(), "first response authorized both operations");
        terminal.resize_and_wait(24,100,"UNSENT_ACTIVE_DRAFT")?;
        terminal.send(b"\x01")?;
        // A cached projection may reopen the first request before the next
        // snapshot. It must stay inert rather than consume the preserved draft.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            terminal.receive();
            let screen = terminal.screen.screen().contents();
            let footer = screen.lines().rev().take(4).collect::<Vec<_>>().join("\n");
            if footer.contains("Permission no longer pending") {
                terminal.wait("Permission no longer pending")?;
                terminal.send(b"\r\t\x1b[B")?;
                ensure!(!controls.join("completed-right").exists());
                let closed = terminal.raw.len();
                terminal.send(b"\x1b")?;
                terminal.wait_fresh_text(closed,"scope: this Goal")?;
                terminal.send(b"\x01")?;
                break;
            }
            if footer.contains("Allow second fixture write") { break; }
            ensure!(Instant::now() < deadline, "permission refresh showed neither the live request nor its stale notice: {screen}");
        }
        terminal.wait("Allow second fixture write")?;
        terminal.send(b"\r")?;
        terminal.redraw_and_wait("Allow second fixture write")?;
        ensure!(head(&socket,&id).await?.workflow.decisions.iter().any(|current| current.id == second.id && current.state == PermissionState::Pending));
        ensure!(!controls.join("completed-right").exists(), "second request reused first selection");
        terminal.wait("Confirm right.txt")?;
        terminal.send(b"\x1b[B\r")?;
        wait_goal(&socket,&id,|goal| controls.join("completed-right").exists()
            && goal.workflow.decisions.iter().any(|current| current.id == second.id && matches!(current.state, PermissionState::Submitted { .. }))).await?;
        let approvals = history(&socket,&id,head(&socket,&id).await?.revision).await?;
        for answered in [&decision,&second] {
            ensure!(approvals.iter().filter(|operation| matches!(&operation.kind,
                OperationKind::Workflow { change } if matches!(change.as_ref(),
                    bitrouter_orchestrator::goal::workflow::Change::Decision {
                        event:bitrouter_orchestrator::goal::decision::Event::Result { decision_id,submitted:true,.. }
                    } if decision_id == &answered.id))).count() == 1, "permission response was not delivered exactly once");
        }
        terminal.resize_and_wait(24,100,"UNSENT_ACTIVE_DRAFT")?;
        terminal.wait("UNSENT_ACTIVE_DRAFT")?;
        terminal.detach()?;
        let detached = head(&socket,&id).await?;
        ensure!(detached.run_intent == RunIntent::Run && running(&detached) == 2);
        ensure!(!detached.workflow.completion.completed_for(1));
        std::fs::write(controls.join("release"), b"finish after terminal detach")?;
        let completed = wait_goal(&socket,&id,|goal| goal.workflow.completion.completed_for(1)).await?;
        ensure!(completed.requirements_revision == 1);
        let requests = backend.received_requests().await.ok_or_else(|| anyhow::anyhow!("provider requests unavailable"))?;
        let generation_requests = requests.iter().filter(|request| request.url.path() == "/v1/chat/completions").count();
        ensure!(completed.accounting.provider_attempts == u64::try_from(generation_requests)?,
            "recorded model attempts do not match actual generation requests");
        ensure!(completed.accounting.in_flight.is_empty());
        ensure!(completed.workflow.tasks.iter().all(|task| task.requirements_revision == 1 && task.executions.len() == 1));
        ensure!(controls.join("started-join").exists());
        ensure!(std::fs::read_to_string(workspace.join("seed.txt"))? == "original input\n");
        ensure!(!workspace.join("left.txt").exists() && !workspace.join("right.txt").exists());
        let mut terminal = Terminal::open(home.path(),true)?;
        terminal.wait("PTY active parallel Goal")?;
        terminal.send(b"\r")?;
        terminal.wait("Verified delivery / requirements 1")?;
        terminal.wait("Satisfaction: complete")?;
        terminal.wait("Managed generation attempts:")?;
        terminal.detach()?;
        Ok::<_,anyhow::Error>(())
    }.await;
    let stop = send_command(&socket, &DaemonCommand::Stop).await;
    if stop.is_err() {
        server.kill().await?;
    }
    let ended = tokio::time::timeout(Duration::from_secs(15), server.wait()).await;
    ensure!(ended.is_ok(), "fixture daemon did not finish cleanup");
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_external_editor_retains_exact_drafts_requires_enter_and_reaps_on_signal() -> Result<()>
{
    use bitrouter_orchestrator::goal::OperationKind;
    let binary =
        example_binary::build("bitrouter", "pty_acp_fixture").map_err(anyhow::Error::msg)?;
    let home = tempfile::tempdir()?;
    let config = home.path().join("bitrouter.yaml");
    let temporary = home.path().join("tmp");
    let capture = home.path().join("editor-input");
    let replacement = home.path().join("editor-replacement");
    let pid_file = home.path().join("editor-pid");
    let release = home.path().join("editor-release");
    std::fs::create_dir(&temporary)?;
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "inherit_defaults":false,
            "server":{"listen":"127.0.0.1:0","skip_auth":true},
            "registry":{"enabled":false},
            "database":{"url":format!("sqlite://{}?mode=rwc",home.path().join("bitrouter.db").display())}
        }))?,
    )?;
    let source = bitrouter::paths::ConfigSource::File(config.clone());
    let cfg = bitrouter::paths::load_config(&source).await?;
    let socket = bitrouter::daemon::socket_path_for(&source, &cfg);
    let mut server = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(["serve", "--config"])
        .arg(&config)
        .env("HOME", home.path())
        .env("BITROUTER_HOME", home.path())
        .current_dir(home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let editor = format!(
        "'{}' --editor",
        binary.to_string_lossy().replace('\'', "'\\''")
    );
    let result = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while directory(&socket).await.is_err() {
            ensure!(server.try_wait()?.is_none(), "editor fixture daemon exited");
            ensure!(tokio::time::Instant::now() < deadline, "editor fixture daemon not ready");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for fail in [false, true] {
            let original = "original draft 中文 👩‍💻\nsecond original line  ";
            let edited = "replacement 中文 👩‍💻\nsecond edited line  ";
            std::fs::write(&replacement, edited)?;
            let mut environment = vec![
                ("VISUAL", std::ffi::OsStr::new(&editor)),
                ("TMPDIR", temporary.as_os_str()),
                ("BITROUTER_PTY_EDITOR_CAPTURE", capture.as_os_str()),
                ("BITROUTER_PTY_EDITOR_CONTENT", replacement.as_os_str()),
            ];
            if fail { environment.push(("BITROUTER_PTY_EDITOR_FAIL", std::ffi::OsStr::new("1"))); }
            let mut terminal = Terminal::open_with_environment(home.path(), true, &environment)?;
            terminal.wait("Enter observe")?;
            terminal.send(b"n")?;
            terminal.wait("New Goal")?;
            terminal.send(format!("\x1b[200~{original}\x1b[201~").as_bytes())?;
            terminal.wait("second original line")?;
            let count = directory(&socket).await?;
            terminal.send(b"\x07")?;
            terminal.wait(if fail { "External editor failed" } else { "External editor returned" })?;
            terminal.wait(if fail { "second original line" } else { "second edited line" })?;
            ensure!(std::fs::read_to_string(&capture)? == original, "new editor changed original bytes");
            ensure!(directory(&socket).await? == count, "editor delegated a new Goal");
            ensure!(!terminal.screen.screen().alternate_screen());
            terminal.send(b"\x1b")?;
            terminal.wait("Enter observe")?;
            let title = if fail { "PTY failed editor Goal" } else { "PTY successful editor Goal" };
            let GoalReply::Accepted { operation } = request(&socket, GoalCommand::Create {
                operation_id:title.replace(' ', "-"), title:title.into(), workspace:home.path().into(),
                objective:"Retain editor inputs without running model or harness work".into(),
                budget_limits:Limits { elapsed_ms:Some(0), managed_token_estimates_micro_usd:None },
            }).await? else { anyhow::bail!("editor Goal creation omitted receipt"); };
            let id = operation.goal_id;
            terminal.send(b"r")?;
            terminal.wait(title)?;
            // The newest Goal is below the previous Goal on the second pass.
            if fail { terminal.send(b"\x1b[B")?; }
            terminal.send(b"\r")?;
            terminal.wait("scope: this Goal")?;
            let goal_draft = format!("current Goal {original}");
            terminal.send(format!("\x1b[200~{goal_draft}\x1b[201~").as_bytes())?;
            terminal.wait("second original line")?;
            let before = head(&socket, &id).await?;
            terminal.send(b"\x07")?;
            terminal.wait(if fail { "External editor failed" } else { "External editor returned" })?;
            terminal.wait(if fail { "second original line" } else { "second edited line" })?;
            ensure!(std::fs::read_to_string(&capture)? == goal_draft, "Goal editor changed original bytes");
            let after = head(&socket, &id).await?;
            ensure!(after.requirements_revision == before.requirements_revision && after.run_intent == before.run_intent);
            ensure!(!history(&socket, &id, after.revision).await?.iter().any(|operation| matches!(operation.kind, OperationKind::Input { .. })), "editor submitted input");
            if fail {
                let recovered = std::fs::read_dir(&temporary)?.collect::<std::io::Result<Vec<_>>>()?;
                ensure!(recovered.len() == 2, "failed editors did not retain both recovery files");
                for file in recovered { ensure!(std::fs::read_to_string(file.path())? == edited); }
            }
            let expected = if fail { goal_draft.as_str() } else { edited };
            terminal.send(b"\r")?;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let current = head(&socket, &id).await?;
                    let inputs = history(&socket, &id, current.revision).await?;
                    if inputs.iter().any(|operation| matches!(&operation.kind, OperationKind::Input { text, task_reference:None } if text == expected)) { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<_,anyhow::Error>(())
            }).await??;
            terminal.detach()?;
        }
        let environment = [
            ("VISUAL", std::ffi::OsStr::new(&editor)),
            ("TMPDIR", temporary.as_os_str()),
            ("BITROUTER_PTY_EDITOR_CAPTURE", capture.as_os_str()),
            ("BITROUTER_PTY_EDITOR_CONTENT", replacement.as_os_str()),
            ("BITROUTER_PTY_EDITOR_PID", pid_file.as_os_str()),
            ("BITROUTER_PTY_EDITOR_HOLD", release.as_os_str()),
        ];
        let mut terminal = Terminal::open_with_environment(home.path(), true, &environment)?;
        terminal.wait("Enter observe")?;
        terminal.send(b"n")?;
        terminal.wait("New Goal")?;
        terminal.send(b"unsent editor signal draft\x07")?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pid_file.exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await?;
        terminal.assert_modes_restored()?;
        ensure!(std::fs::read_to_string(&capture)? == "unsent editor signal draft");
        terminal.signal_exit("-TERM").await?;
        let pid = std::fs::read_to_string(&pid_file)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = Command::new("ps").args(["-o", "state=", "-p", pid.trim()]).output().await?;
                if state.stdout.is_empty() { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_,anyhow::Error>(())
        }).await??;
        ensure!(directory(&socket).await? == 2, "signal exit delegated a draft");
        Ok::<_,anyhow::Error>(())
    }.await;
    let release_result = std::fs::write(&release, "release");
    let stop = send_command(&socket, &DaemonCommand::Stop).await;
    let status = tokio::time::timeout(Duration::from_secs(10), server.wait()).await;
    result?;
    release_result?;
    ensure!(matches!(stop?, DaemonResponse::Ok));
    ensure!(status??.success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_and_code_open_goals_without_work_and_preserve_unsent_drafts() -> Result<()> {
    for inherit_defaults in [false, true] {
        verify_goal_entries_with_defaults(inherit_defaults).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_directory_resize_defers_backlog_and_isolates_native_inspection() -> Result<()> {
    let binary =
        example_binary::build("bitrouter", "pty_acp_fixture").map_err(anyhow::Error::msg)?;
    let home = tempfile::tempdir()?;
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "inherit_defaults":true,"registry":{"enabled":false},"server":{"listen":"127.0.0.1:0","skip_auth":true},
            "database":{"url":format!("sqlite://{}?mode=rwc",home.path().join("goals.db").display())},
            "agents":{"stub":{"name":"stub","transport":{"type":"stdio","command":binary,"args":[],"env":{
                "BITROUTER_PTY_SCENARIO":"minimal","BITROUTER_PTY_CAPTURE":home.path().join("native-capture"),"BITROUTER_PTY_CONTROL":home.path().join("native-control.sock")
            }}}}
        }))?,
    )?;
    let source = bitrouter::paths::ConfigSource::File(config.clone());
    let cfg = bitrouter::paths::load_config(&source).await?;
    let socket = bitrouter::daemon::socket_path_for(&source, &cfg);
    let mut server = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(["serve", "--config"])
        .arg(&config)
        .env("HOME", home.path())
        .env("BITROUTER_HOME", home.path())
        .current_dir(home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let result = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while directory(&socket).await.is_err() {
            ensure!(server.try_wait()?.is_none(), "isolation daemon exited");
            ensure!(
                tokio::time::Instant::now() < deadline,
                "isolation daemon unavailable"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let GoalReply::Accepted { operation } = request(
            &socket,
            GoalCommand::Create {
                operation_id: "history-isolation-goal".into(),
                title: "History isolation Goal".into(),
                workspace: home.path().into(),
                objective: "Retain this Goal and its events without model or worker execution"
                    .into(),
                budget_limits: Limits {
                    elapsed_ms: Some(0),
                    managed_token_estimates_micro_usd: None,
                },
            },
        )
        .await?
        else {
            anyhow::bail!("isolation Goal receipt missing");
        };
        let id = operation.goal_id;
        let mut terminal = Terminal::open(home.path(), true)?;
        terminal.wait("History isolation Goal")?;
        terminal.send(b"\r")?;
        terminal.wait("scope: this Goal")?;
        terminal.send(b"ISOLATED_UNSENT_DRAFT")?;
        terminal.wait("ISOLATED_UNSENT_DRAFT")?;
        for visit in 0..3 {
            terminal.send(b"\x1b[1;5D")?;
            terminal.redraw_and_wait("Enter observe")?;
            if visit == 2 {
                terminal.suspend_resize_resume().await?;
                terminal.redraw_and_wait("Enter observe")?;
            }
            terminal.send(b"\r")?;
            terminal.redraw_and_wait("ISOLATED_UNSENT_DRAFT")?;
        }
        terminal.send(b"\x1b[1;5D")?;
        terminal.wait("Enter observe")?;
        let hidden = terminal.raw.len();
        for number in 0..90 {
            let marker = format!("GOAL_BACKLOG_{number:03}");
            let mut accepted = false;
            for _ in 0..4 {
                let current = head(&socket, &id).await?;
                let response = send_command(
                    &socket,
                    &DaemonCommand::Goals {
                        protocol: bitrouter_orchestrator::goal::PROTOCOL_VERSION,
                        command: GoalCommand::SetBudget {
                            operation_id: marker.clone(),
                            goal_id: id.clone(),
                            expected_revision: current.revision,
                            limits: current.budget.limits,
                        },
                    },
                )
                .await?;
                match response {
                    DaemonResponse::Goals {
                        response: Ok(GoalReply::Accepted { operation }),
                        ..
                    } => {
                        ensure!(operation.id == marker && operation.goal_id == id);
                        accepted = true;
                        break;
                    }
                    DaemonResponse::Goals {
                        response: Err(bitrouter_orchestrator::goal::GoalError::Conflict),
                        ..
                    } => {}
                    other => anyhow::bail!("budget marker not accepted: {other:?}"),
                }
            }
            ensure!(accepted, "budget marker never committed");
        }
        let launched = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args([
                "run",
                "stub",
                "background-isolation-fixture",
                "--direct",
                "--background",
                "--json",
                "--config",
            ])
            .arg(&config)
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("BITROUTER_HOME", home.path())
            .output()
            .await?;
        ensure!(
            launched.status.success(),
            "native background failed: {}",
            String::from_utf8_lossy(&launched.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&launched.stdout)?;
        let run = report["agent_run_id"]
            .as_str()
            .context("native run identity missing")?
            .to_owned();
        let current = head(&socket, &id).await?;
        let committed = history(&socket, &id, current.revision).await?;
        ensure!(
            committed
                .iter()
                .filter(|operation| operation.id.starts_with("GOAL_BACKLOG_"))
                .count()
                == 90
        );
        let native_args = [
            std::ffi::OsString::from("agents"),
            std::ffi::OsString::from("attach"),
            run.clone().into(),
            std::ffi::OsString::from("--config"),
            config.clone().into_os_string(),
        ];
        let mut inspector = Terminal::open_args(home.path(), &native_args, &[])?;
        inspector.wait("BACKGROUND_PRIVATE_OUTPUT")?;
        inspector.wait_alternate(true)?;
        inspector.signal_exit("-TERM").await?;
        terminal.resize_and_wait(20, 60, "Goals")?;
        terminal.resize_and_wait(24, 100, "Goals")?;
        terminal.send(b"/History")?;
        terminal.redraw_and_wait("Search this page / workflow")?;
        terminal.send(b"\r")?;
        terminal.redraw_and_wait("Enter observe")?;
        ensure!(
            !String::from_utf8_lossy(&terminal.raw[hidden..]).contains("GOAL_BACKLOG_"),
            "hidden Goal history leaked during directory resize"
        );
        ensure!(
            !String::from_utf8_lossy(&terminal.raw).contains("BACKGROUND_PRIVATE_OUTPUT"),
            "native output entered Goal UI"
        );
        let caught_up = terminal.raw.len();
        terminal.send(b"\r")?;
        terminal.wait("GOAL_BACKLOG_089")?;
        terminal.wait("ISOLATED_UNSENT_DRAFT")?;
        for number in 0..90 {
            let marker = format!("GOAL_BACKLOG_{number:03}");
            ensure!(
                terminal.raw[caught_up..]
                    .windows(marker.len())
                    .filter(|bytes| *bytes == marker.as_bytes())
                    .count()
                    == 1,
                "catch-up repeated or omitted {marker}"
            );
        }
        terminal.send(b"\x0c")?;
        terminal.wait("ISOLATED_UNSENT_DRAFT")?;
        ensure!(
            !String::from_utf8_lossy(&terminal.raw).contains("BACKGROUND_PRIVATE_OUTPUT"),
            "native inspector replayed into Goal scrollback"
        );
        let raw = String::from_utf8_lossy(&terminal.raw);
        ensure!(
            !raw.contains("\x1b[?1049h") && !raw.contains("\x1b[3J"),
            "Goals navigation replaced or cleared normal scrollback"
        );
        // vt100 0.15 subtracts the scrollback offset from its drawing-row count.
        // Enlarge only the held parser grid to inspect all retained rows safely;
        // the actual PTY remains at its tested size and receives no output here.
        let (rows, cols) = terminal.screen.screen().size();
        terminal.screen.set_size(2024, cols);
        terminal.screen.set_scrollback(2000);
        let scrollback = terminal.screen.screen().contents();
        terminal.screen.set_scrollback(0);
        terminal.screen.set_size(rows, cols);
        ensure!(
            !scrollback.contains("Enter observe")
                && !scrollback.contains("Search this page / workflow"),
            "directory controls entered Goal history"
        );
        terminal.detach()?;
        let reader = bitrouter::supervisor::authorize(
            &socket,
            "goal-isolation-reader",
            [bitrouter::supervisor::SessionScope::Peek]
                .into_iter()
                .collect(),
        )
        .await?;
        let bitrouter::supervisor::SessionResponse::Runs { runs } = bitrouter::supervisor::request(
            &socket,
            &reader,
            bitrouter::supervisor::SessionCommand::PeekAll,
        )
        .await?
        else {
            anyhow::bail!("native snapshots missing");
        };
        ensure!(
            runs.iter().any(|snapshot| snapshot.run_id == run
                && snapshot.process == bitrouter::supervisor::ProcessState::Running
                && snapshot.lease.is_none()),
            "Goal/inspector exit stopped or claimed native run"
        );
        let unchanged = head(&socket, &id).await?;
        ensure!(
            unchanged.workflow.tasks.is_empty()
                && unchanged.accounting.provider_attempts == 0
                && unchanged.requirements_revision == 1
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stop = send_command(&socket, &DaemonCommand::Stop).await;
    if stop.is_err() {
        server.kill().await?;
    }
    tokio::time::timeout(Duration::from_secs(15), server.wait()).await??;
    result
}

async fn verify_goal_entries_with_defaults(inherit_defaults: bool) -> Result<()> {
    let home = tempfile::tempdir()?;
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "inherit_defaults":inherit_defaults,
            "server":{"listen":"127.0.0.1:0","skip_auth":true},
            "registry":{"enabled":false},
            "database":{"url":format!("sqlite://{}?mode=rwc",home.path().join("bitrouter.db").display())}
        }))?,
    )?;
    let source = bitrouter::paths::ConfigSource::File(config.clone());
    let cfg = bitrouter::paths::load_config(&source).await?;
    let socket = bitrouter::daemon::socket_path_for(&source, &cfg);
    let mut server = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(["serve", "--config"])
        .arg(&config)
        .env("HOME", home.path())
        .env("BITROUTER_HOME", home.path())
        .current_dir(home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let result = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while directory(&socket).await.is_err() {
            ensure!(server.try_wait()?.is_none(), "fixture daemon exited");
            ensure!(
                tokio::time::Instant::now() < deadline,
                "fixture daemon not ready"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for code in [false, true] {
            let mut terminal = Terminal::open(home.path(), code)?;
            terminal.wait("No matching Goals")?;
            ensure!(!terminal.screen.screen().alternate_screen());
            ensure!(directory(&socket).await? == 0);
            terminal.send(b"n")?;
            terminal.wait("New Goal")?;
            if code && !inherit_defaults {
                let mut resize_draft = String::from("RESIZE_KEYS_");
                terminal.send(resize_draft.as_bytes())?;
                terminal.wait(&resize_draft)?;
                for index in 0..32 {
                    let (rows, cols) = if index % 2 == 0 { (25, 110) } else { (24, 100) };
                    terminal.resize(rows, cols)?;
                    terminal.send(b"x")?;
                    resize_draft.push('x');
                    // No redraw or additional input may wake a stranded key.
                    terminal.wait(&resize_draft)?;
                }
                ensure!(
                    directory(&socket).await? == 0,
                    "resize input delegated work"
                );
            }
            let draft = "中文 👩‍💻 draft\nsecond line";
            terminal.send(format!("\x1b[200~{draft}\x1b[201~").as_bytes())?;
            terminal.wait("second line")?;
            ensure!(directory(&socket).await? == 0, "paste delegated work");
            terminal.send(b"\x1b")?;
            terminal.wait("No matching Goals")?;
            terminal.send(b"n")?;
            terminal.wait("second line")?;
            terminal.resize(10, 35)?;
            terminal.wait("Resize to at least")?;
            terminal.send(b"\r")?;
            ensure!(
                directory(&socket).await? == 0,
                "small viewport submitted draft"
            );
            terminal.resize(24, 100)?;
            terminal.wait("second line")?;
            terminal.suspend_resize_resume().await?;
            terminal.wait("second line")?;
            ensure!(!terminal.screen.screen().alternate_screen());
            ensure!(
                directory(&socket).await? == 0,
                "suspend/resume delegated the draft"
            );
            terminal.detach()?;
            ensure!(directory(&socket).await? == 0, "detaching submitted draft");
        }
        ensure!(!home.path().join("goal-workspaces").exists());
        // Zero elapsed limits keep these observation fixtures unfinished.
        // This does not simulate a running branch or a model verdict.
        let mut ids = Vec::new();
        for title in ["PTY Goal Left", "PTY Goal Right"] {
            let GoalReply::Accepted { operation } = request(
                &socket,
                GoalCommand::Create {
                    operation_id: title.replace(' ', "-"),
                    title: title.into(),
                    workspace: home.path().into(),
                    objective: "Retain this unfinished Goal for observation and draft isolation"
                        .into(),
                    budget_limits: Limits {
                        elapsed_ms: Some(0),
                        managed_token_estimates_micro_usd: None,
                    },
                },
            )
            .await?
            else {
                anyhow::bail!("creation omitted receipt");
            };
            ids.push(operation.goal_id);
        }
        let mut before = Vec::new();
        for id in &ids {
            before.push(head(&socket, id).await?);
        }
        let mut terminal = Terminal::open(home.path(), true)?;
        terminal.wait("PTY Goal Left")?;
        terminal.wait("PTY Goal Right")?;
        terminal.send(b"\r")?;
        terminal.wait("scope: this Goal")?;
        terminal.send(b"GOAL_FIRST_DRAFT")?;
        terminal.wait("GOAL_FIRST_DRAFT")?;
        terminal.send(b"\x1b[1;5C")?;
        terminal.wait("Workflow")?;
        terminal.send(b"branch edit\r\x10\x18\x01\x12\x02")?;
        terminal.send(b"\x06")?;
        terminal.wait_alternate(true)?;
        terminal.send(b"\x1b")?;
        terminal.wait_alternate(false)?;
        terminal.wait("Workflow · read only")?;
        ensure!(!terminal.screen.screen().alternate_screen());
        terminal.send(b"\x1b")?;
        terminal.wait("GOAL_FIRST_DRAFT")?;
        terminal.send(b"\x1b[1;5D")?;
        terminal.wait("Enter observe")?;
        // The directory contains two Goals; Down changes observation only.
        terminal.send(b"\x1b[B\r")?;
        terminal.wait("scope: this Goal")?;
        terminal.send(b"GOAL_SECOND_DRAFT")?;
        terminal.wait("GOAL_SECOND_DRAFT")?;
        ensure!(
            !terminal
                .screen
                .screen()
                .contents()
                .contains("GOAL_FIRST_DRAFT")
        );
        terminal.send(b"\x1b[1;5D\x1b[A\r")?;
        terminal.wait("GOAL_FIRST_DRAFT")?;
        ensure!(
            !terminal
                .screen
                .screen()
                .contents()
                .contains("GOAL_SECOND_DRAFT")
        );
        terminal.detach()?;
        for (code, signal) in [(false, "-INT"), (true, "-TERM")] {
            let mut terminal = Terminal::open(home.path(), code)?;
            terminal.wait("PTY Goal Left")?;
            terminal.send(b"\r")?;
            terminal.wait("scope: this Goal")?;
            terminal.send(b"unsent signal draft")?;
            terminal.wait("unsent signal draft")?;
            terminal.send(b"\x1b[1;5C\x06")?;
            terminal.wait_alternate(true)?;
            terminal.signal_exit(signal).await?;
        }
        for (id, original) in ids.iter().zip(before) {
            let actual = head(&socket, id).await?;
            ensure!(actual.objective == original.objective);
            ensure!(actual.requirements_revision == original.requirements_revision);
            ensure!(actual.run_intent == original.run_intent);
            ensure!(
                actual.inputs == original.inputs,
                "read-only keys submitted Goal input"
            );
            ensure!(actual.budget.limits == original.budget.limits);
            ensure!(actual.workflow.tasks.is_empty());
        }
        ensure!(directory(&socket).await? == 2);
        Result::<()>::Ok(())
    }
    .await;
    server.kill().await?;
    let _ = server.wait().await?;
    result
}
