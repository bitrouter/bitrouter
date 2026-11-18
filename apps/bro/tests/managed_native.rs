//! M0's managed consumer scenarios, using the standalone foreground worker, its
//! real workspace tools, durable checkpoints, and the public SDK HTTP stream.
#[path = "support/binary.rs"]
mod binary;
#[path = "support/managed_model.rs"]
mod model;

use anyhow::{Context, Result, ensure};
use model::{Model, answer, calls};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio::process::Command;

struct Consumer {
    _temp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    endpoint: String,
    key: Option<std::ffi::OsString>,
}

impl Consumer {
    fn new(endpoint: &str) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let work = temp.path().join("work");
        let home = temp.path().join("data");
        fs::create_dir(&work)?;
        Ok(Self {
            _temp: temp,
            home,
            work,
            endpoint: endpoint.into(),
            key: Some("scoped-test-key".into()),
        })
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(binary::bro());
        command
            .kill_on_drop(true)
            .current_dir(&self.work)
            .env_clear()
            .env("BRO_HOME", &self.home)
            .args(["--endpoint", &self.endpoint])
            .args(args);
        if let Some(key) = &self.key {
            command.env("BITROUTER_API_KEY", key);
        }
        command
    }
    async fn run(&self, args: &[&str], success: bool) -> Result<Value> {
        let out =
            tokio::time::timeout(Duration::from_secs(540), self.command(args).output()).await??;
        ensure!(
            out.status.success() == success,
            "BRO {:?}: {} {}",
            args.first(),
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .last()
                .unwrap_or_default()
        );
        let line = out
            .stdout
            .split(|b| *b == b'\n')
            .rfind(|l| !l.is_empty())
            .context("no result")?;
        Ok(serde_json::from_slice(line)?)
    }
}

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v[name].as_str().with_context(|| format!("missing {name}"))
}

fn spawn(name: String) -> (String, &'static str, Value) {
    (
        format!("assign-{name}"),
        "spawn_agent",
        json!({"task":{"text":name}}),
    )
}
fn read(id: String, path: &str) -> (String, &'static str, Value) {
    (id, "read", json!({"path":path}))
}
fn user(messages: &[Value]) -> Result<&str> {
    field(
        messages
            .iter()
            .rfind(|m| m["role"] == "user")
            .context("missing task")?,
        "content",
    )
}
fn agent(messages: &[Value]) -> Result<&str> {
    messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find_map(|s| {
            s.strip_prefix("You are agent ")
                .and_then(|s| s.split_once(" for this task.").map(|(id, _)| id))
        })
        .context("missing agent attribution")
}
fn tool_messages(messages: &[Value]) -> impl Iterator<Item = &Value> {
    messages.iter().filter(|m| m["role"] == "tool")
}

#[tokio::test]
async fn full_tree_runs_four_models_and_delivers_each_conclusion_once() -> Result<()> {
    let barrier = Arc::new((Mutex::new(0), Condvar::new()));
    let model = Model::start(move |m| {
        let task = user(m)?;
        let results: Vec<_> = tool_messages(m).collect();
        if results.is_empty() && task.starts_with("leaf-") {
            let mut entered = barrier.0.lock().map_err(|_| anyhow::anyhow!("barrier"))?;
            *entered += 1;
            barrier.1.notify_all();
            let (_guard, timeout) = barrier
                .1
                .wait_timeout_while(entered, Duration::from_secs(30), |n| *n < 4)
                .map_err(|_| anyhow::anyhow!("barrier"))?;
            ensure!(
                !timeout.timed_out(),
                "four distinct child sends never entered"
            );
        }
        Ok(match (task, results.len()) {
            ("tree", 0) => calls((0..28).map(|n| spawn(format!("leaf-{n}"))).collect()),
            ("tree", 28) => {
                let mut unique = HashSet::new();
                for message in results {
                    let note: Value = serde_json::from_str(field(message, "content")?)?;
                    ensure!(
                        note["parent_id"] == agent(m)?
                            && note["status"] == "completed"
                            && unique.insert(field(&note, "agent_id")?.to_owned())
                            && note["answer"]
                                == message["tool_call_id"]
                                    .as_str()
                                    .context("call")?
                                    .trim_start_matches("assign-")
                    );
                }
                calls(vec![spawn("excess-width".into())])
            }
            ("tree", 29) => {
                ensure!(field(results[28], "content")?.contains("agent count limit"));
                answer("joined all children")
            }
            ("leaf-0", 0) => calls(vec![spawn("depth-2".into())]),
            ("depth-2", 0) => calls(vec![spawn("depth-3".into())]),
            ("depth-3", 0) => calls(vec![spawn("depth-4".into())]),
            ("depth-4", 0) => calls(vec![spawn("excess-depth".into())]),
            ("depth-4", 1) => {
                ensure!(field(results[0], "content")?.contains("depth limit"));
                answer(task)
            }
            ("leaf-0" | "depth-2" | "depth-3", 1) => {
                let note: Value = serde_json::from_str(field(results[0], "content")?)?;
                ensure!(note["parent_id"] == agent(m)? && note["status"] == "completed");
                answer(task)
            }
            ("leaf-1", 0) => calls(
                (0..8)
                    .map(|n| read(format!("read-{n}"), "evidence"))
                    .collect(),
            ),
            ("leaf-1", 8) => {
                ensure!(results.iter().all(|m| {
                    m["content"].as_str().is_some_and(|s| {
                        s.ends_with(&format!("{}\n", "actual workspace evidence ".repeat(1960)))
                    })
                }));
                answer(task)
            }
            (leaf, 0) if leaf.starts_with("leaf-") => answer(task),
            _ => anyhow::bail!(
                "unexpected model dispatch for {task} with {} results",
                results.len()
            ),
        })
    })?;
    let cli = Consumer::new(&model.endpoint)?;
    fs::write(
        cli.work.join("evidence"),
        "actual workspace evidence ".repeat(1960),
    )?;
    let done = cli
        .run(&["task", "run", "--managed", "--read-only", "tree"], true)
        .await?;
    ensure!(done["final_answer"] == "joined all children");
    let id = field(&done, "thread_id")?;
    let turn = field(&done, "turn_id")?;
    let history = cli.run(&["thread", "history", id], true).await?;
    let mut ids = HashSet::new();
    let mut receipts = HashSet::new();
    fn verify(t: &Value, ids: &mut HashSet<String>, receipts: &mut HashSet<String>) -> Result<()> {
        ensure!(ids.insert(field(t, "thread_id")?.to_owned()));
        let turn = &t["turns"][0];
        ensure!(turn["outcome"]["status"] == "completed");
        for step in turn["steps"].as_array().context("steps")? {
            ensure!(receipts.insert(field(step, "execution_id")?.to_owned()));
        }
        if let Some(children) = turn["children"].as_object() {
            for child in children.values() {
                verify(child, ids, receipts)?;
            }
        }
        Ok(())
    }
    verify(&history, &mut ids, &mut receipts)?;
    ensure!(ids.len() == 32 && model.peak.load(Ordering::SeqCst) == 4);
    ensure!(
        cli.run(&["thread", "resume", id, "--turn", turn], true)
            .await?
            == done
    );
    ensure!(cli.run(&["thread", "history", id], true).await? == history);
    // A well-formed JSON ledger still needs a bound child and a matching
    // conclusion. None of these drifted terminal checkpoints can authorize work.
    let path = cli.home.join("threads").join(format!("{id}.json"));
    let child = history["turns"][0]["children"]
        .as_object()
        .context("children")?
        .keys()
        .next()
        .context("first child")?
        .clone();
    for fault in ["missing", "parent", "conclusion"] {
        let mut stale = history.clone();
        let children = stale["turns"][0]["children"]
            .as_object_mut()
            .context("child map")?;
        match fault {
            "missing" => {
                children.remove(&child);
            }
            "parent" => {
                children[&child]["parent_id"] = json!(uuid::Uuid::new_v4());
            }
            _ => {
                children[&child]["turns"][0]["outcome"]["final_answer"] = json!("stale conclusion");
            }
        }
        fs::write(&path, serde_json::to_vec(&stale)?)?;
        let refused = cli
            .command(&["thread", "resume", id, "--turn", turn])
            .output()
            .await?;
        ensure!(
            !refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("child")
        );
    }
    fs::write(path, serde_json::to_vec(&history)?)?;
    let received = model.finish()?;
    let senders = received
        .iter()
        .map(|r| agent(r["messages"].as_array().context("messages")?).map(str::to_owned))
        .collect::<Result<HashSet<_>>>()?;
    ensure!(ids == senders && received.len() == receipts.len() && received.len() == 39);
    Ok(())
}

#[tokio::test]
async fn large_cold_checkpoint_keeps_multiturn_context_and_sixteen_step_tool_loop() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(540), large_checkpoint_continuations())
        .await
        .context("three-turn cold recovery deadline")?
}

async fn large_checkpoint_continuations() -> Result<()> {
    // Keep substantial private evidence in individual child contexts. The root
    // gets their conclusions; a restart must not send their transcripts upstream.
    let model = Model::start(|m| {
        let prompt = user(m)?;
        let current = m
            .iter()
            .rposition(|x| x["role"] == "user")
            .context("user")?;
        let results: Vec<_> = tool_messages(&m[current..]).collect();
        if let Some(worker) = prompt.strip_prefix("inspect-part-") {
            let worker: usize = worker.parse()?;
            if results.is_empty() {
                return Ok(calls(
                    (0..6)
                        .map(|part| read(format!("file-{part}"), &format!("part-{worker}-{part}")))
                        .collect(),
                ));
            }
            ensure!(results.len() == 6);
            for (part, result) in results.iter().enumerate() {
                let value = field(result, "content")?;
                ensure!(
                    value.contains(&format!("section {worker}/{part} entry 0000"))
                        && value.contains(&format!("section {worker}/{part} entry 0859")),
                    "incomplete evidence"
                );
            }
            return Ok(answer(&format!("verified partition {worker}")));
        }
        if prompt.starts_with("context ") {
            if results.is_empty() {
                return Ok(calls(
                    (0..8).map(|i| spawn(format!("inspect-part-{i}"))).collect(),
                ));
            }
            ensure!(results.len() >= 8);
            for (worker, note) in results[..8].iter().enumerate() {
                let note: Value = serde_json::from_str(field(note, "content")?)?;
                ensure!(
                    note["answer"] == format!("verified partition {worker}")
                        && note["parent_id"] == agent(m)?
                );
            }
            for r in &results[8..] {
                ensure!(field(r, "content")?.contains(&"retained evidence ".repeat(450)));
            }
            return if results.len() < 14 {
                Ok(calls(vec![read(
                    format!("cold-read-{}", results.len() - 8),
                    "evidence",
                )]))
            } else {
                ensure!(results.len() == 14);
                Ok(answer("initial evidence complete"))
            };
        }
        ensure!(
            m.iter()
                .any(|v| v["role"] == "assistant" && v["content"] == "initial evidence complete")
        );
        ensure!(
            !m.iter().any(|v| v["content"]
                .as_str()
                .is_some_and(|s| s.contains("entry 0859"))),
            "private child transcripts leaked into parent context"
        );
        match prompt {
            "continue" => {
                for r in &results {
                    ensure!(field(r, "content")?.contains(&"retained evidence ".repeat(250)));
                }
                if results.len() < 16 {
                    Ok(calls(vec![read(
                        format!("long-read-{}", results.len()),
                        "long-evidence",
                    )]))
                } else {
                    ensure!(results.len() == 16);
                    Ok(answer("sixteen reads complete"))
                }
            }
            "continue after restart" => {
                ensure!(
                    m.iter()
                        .any(|v| v["role"] == "assistant"
                            && v["content"] == "sixteen reads complete")
                );
                ensure!(
                    tool_messages(m)
                        .filter(|r| r["tool_call_id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with("long-read-")))
                        .count()
                        == 16
                );
                Ok(answer("cold continuation complete"))
            }
            _ => anyhow::bail!("unexpected task"),
        }
    })?;
    let cli = Consumer::new(&model.endpoint)?;
    fs::write(cli.work.join("evidence"), "retained evidence ".repeat(450))?;
    fs::write(
        cli.work.join("long-evidence"),
        "retained evidence ".repeat(250),
    )?;
    for worker in 0..8 {
        for part in 0..6 {
            let data = (0..860)
                .map(|row| {
                    format!(
                        "section {worker}/{part} entry {row:04} inspected source record {row:04}\n"
                    )
                })
                .collect::<String>();
            fs::write(cli.work.join(format!("part-{worker}-{part}")), data)?;
        }
    }
    let created = cli
        .run(&["thread", "create", "--managed", "--read-only"], true)
        .await?;
    let id = field(&created, "thread_id")?;
    let prompt = cli.work.join("prompt");
    fs::write(&prompt, "context ".repeat(5000))?;
    let first = cli
        .run(
            &[
                "thread",
                "start",
                id,
                "--prompt-file",
                prompt.to_str().context("path")?,
                "--request-id",
                "first",
            ],
            true,
        )
        .await?;
    ensure!(first["status"] == "completed");
    let checkpoint = cli.home.join("threads").join(format!("{id}.json"));
    let size = fs::metadata(&checkpoint)?.len();
    ensure!(
        size > 2 * 1024 * 1024,
        "first native checkpoint is only {size} bytes"
    );
    let before: Value = serde_json::from_reader(fs::File::open(&checkpoint)?)?;
    let second = cli
        .run(
            &["thread", "start", id, "continue", "--request-id", "second"],
            true,
        )
        .await?;
    ensure!(second["final_answer"] == "sixteen reads complete");
    let after: Value = serde_json::from_reader(fs::File::open(&checkpoint)?)?;
    ensure!(after["turns"][0] == before["turns"][0]);
    let steps = after["turns"][1]["steps"]
        .as_array()
        .context("loop steps")?;
    ensure!(
        steps.len() == 17
            && steps
                .iter()
                .map(|s| s["state"]["response"]["calls"]
                    .as_array()
                    .map_or(0, Vec::len))
                .sum::<usize>()
                == 16
    );
    fs::remove_file(cli.work.join("long-evidence"))?;
    fs::remove_file(cli.work.join("evidence"))?;
    for worker in 0..8 {
        for part in 0..6 {
            fs::remove_file(cli.work.join(format!("part-{worker}-{part}")))?;
        }
    }
    ensure!(
        cli.run(
            &["thread", "resume", id, "--turn", field(&second, "turn_id")?],
            true
        )
        .await?
            == second
    );
    let third = cli
        .run(
            &[
                "thread",
                "start",
                id,
                "continue after restart",
                "--request-id",
                "third",
            ],
            true,
        )
        .await?;
    ensure!(third["final_answer"] == "cold continuation complete");
    ensure!(
        cli.run(
            &[
                "thread",
                "start",
                id,
                "continue after restart",
                "--request-id",
                "third"
            ],
            true
        )
        .await?["turn_id"]
            == third["turn_id"]
    );
    let cold = cli.run(&["thread", "history", id], true).await?;
    ensure!(cold["turns"][0] == before["turns"][0] && cold["turns"][1] == after["turns"][1]);
    let received = model.finish()?;
    let peak_request = received
        .iter()
        .map(serde_json::to_vec)
        .collect::<serde_json::Result<Vec<_>>>()?
        .iter()
        .map(Vec::len)
        .max()
        .context("requests")?;
    eprintln!(
        "first checkpoint {size} bytes, largest inference request {peak_request} bytes, {} sends across three turns",
        received.len()
    );
    ensure!(
        received.len() == 8 + 16 + 17 + 1,
        "unexpected number of paid sends: {}",
        received.len()
    );
    Ok(())
}

#[tokio::test]
async fn active_child_cancellation_fences_paid_dispatch_and_overlap_on_restart() -> Result<()> {
    active_child(false).await?;
    active_child(true).await
}

async fn active_child(kill_worker: bool) -> Result<()> {
    let (entered, mut active) = tokio::sync::mpsc::channel(1);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let gate = release.clone();
    let model = Model::start(move |m| match user(m)? {
        "delegate" => Ok(calls(vec![spawn("held".into())])),
        "held" => {
            entered.blocking_send(()).context("test lost child")?;
            let lock = gate.0.lock().map_err(|_| anyhow::anyhow!("release"))?;
            let (_guard, timeout) = gate
                .1
                .wait_timeout_while(lock, Duration::from_secs(30), |open| !*open)
                .map_err(|_| anyhow::anyhow!("release"))?;
            ensure!(!timeout.timed_out());
            Ok(answer("upstream completed after cancellation"))
        }
        _ => anyhow::bail!("unexpected request"),
    })?;
    let cli = Consumer::new(&model.endpoint)?;
    let created = cli.run(&["thread", "create", "--managed"], true).await?;
    let id = field(&created, "thread_id")?;
    let mut worker = cli
        .command(&["thread", "start", id, "delegate", "--request-id", "active"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(15), active.recv())
        .await?
        .context("child never dispatched")?;
    let snapshot = cli.run(&["thread", "show", id], true).await?;
    ensure!(snapshot["status"] == "running");
    let turn = field(&snapshot, "turn_id")?;
    let replay = cli
        .run(
            &["thread", "start", id, "delegate", "--request-id", "active"],
            true,
        )
        .await?;
    ensure!(replay["turn_id"] == turn && replay["status"] == "running");
    ensure!(
        !cli.command(&["thread", "start", id, "changed", "--request-id", "active"])
            .output()
            .await?
            .status
            .success()
    );
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = cli.run(&["thread", "history", id], true).await?;
            if snapshot["turns"][0]["children"]
                .as_object()
                .and_then(|c| c.values().next())
                .is_some_and(|t| t["turns"][0]["steps"][0]["execution_id"] == "exec-2")
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    if kill_worker {
        worker.kill().await?;
        ensure!(cli.run(&["thread", "show", id], true).await?["status"] == "recovery_required");
    }
    let cancelled = cli
        .run(&["thread", "cancel", id, "--turn", turn], true)
        .await?;
    ensure!(cancelled["status"] == "recovery_required");
    ensure!(!worker.wait().await?.success());
    *release.0.lock().map_err(|_| anyhow::anyhow!("release"))? = true;
    release.1.notify_all();
    let history = cli.run(&["thread", "history", id], true).await?;
    let child = history["turns"][0]["children"]
        .as_object()
        .context("children")?
        .values()
        .next()
        .context("child")?;
    ensure!(
        child["turns"][0]["outcome"]["status"] == "recovery_required"
            && child["turns"][0]["steps"][0]["execution_id"] == "exec-2"
            && child["turns"][0]["steps"][0]["state"]["record"] == "model_request"
    );
    ensure!(
        cli.run(&["thread", "resume", id, "--turn", turn], false)
            .await?["status"]
            == "recovery_required"
    );
    ensure!(cli.run(&["thread", "history", id], true).await? == history);
    ensure!(
        !cli.command(&["task", "run", "overlapping"])
            .output()
            .await?
            .status
            .success()
    );
    ensure!(model.finish()?.len() == 2);
    Ok(())
}

#[tokio::test]
async fn approved_write_reads_and_continues_then_cancels_the_next_approval() -> Result<()> {
    let model = Model::start(|m| {
        let current = m
            .iter()
            .rposition(|v| v["role"] == "user")
            .context("prompt")?;
        let results: Vec<_> = tool_messages(&m[current..]).collect();
        Ok(match (user(m)?, results.len()) {
            ("write and verify", 0) => calls(vec![(
                "write-first".into(),
                "write",
                json!({"path":"note","content":"first contents"}),
            )]),
            ("write and verify", 1) => calls(vec![read("check-first".into(), "note")]),
            ("write and verify", 2) => {
                ensure!(field(results[1], "content")?.contains("first contents"));
                answer("first contents verified")
            }
            ("read again", 0) => {
                ensure!(
                    m.iter()
                        .any(|v| v["role"] == "assistant"
                            && v["content"] == "first contents verified")
                );
                calls(vec![read("check-second".into(), "note")])
            }
            ("read again", 1) => {
                ensure!(field(results[0], "content")?.contains("externally edited"));
                answer("new contents verified")
            }
            ("cancel this write", 0) => calls(vec![(
                "write-cancel".into(),
                "write",
                json!({"path":"note","content":"unapproved"}),
            )]),
            _ => anyhow::bail!("unexpected inference"),
        })
    })?;
    let cli = Consumer::new(&model.endpoint)?;
    let first = cli.run(&["task", "run", "write and verify"], true).await?;
    let id = field(&first, "thread_id")?;
    let turn = field(&first, "turn_id")?;
    let request = field(&first["pending_input"], "request_id")?;
    ensure!(!cli.work.join("note").exists());
    let completed = cli
        .run(
            &[
                "thread",
                "answer",
                id,
                "--turn",
                turn,
                "--request",
                request,
                "allow",
            ],
            true,
        )
        .await?;
    ensure!(
        completed["final_answer"] == "first contents verified"
            && fs::read_to_string(cli.work.join("note"))? == "first contents"
    );
    fs::write(cli.work.join("note"), "externally edited")?;
    ensure!(
        cli.run(&["thread", "resume", id, "--turn", turn], true)
            .await?
            == completed
    );
    let next = cli
        .run(
            &["thread", "start", id, "read again", "--request-id", "next"],
            true,
        )
        .await?;
    ensure!(next["final_answer"] == "new contents verified");
    ensure!(
        cli.run(
            &["thread", "start", id, "read again", "--request-id", "next"],
            true
        )
        .await?["turn_id"]
            == next["turn_id"]
    );
    let pending = cli
        .run(&["thread", "start", id, "cancel this write"], true)
        .await?;
    let cancelled = field(&pending, "turn_id")?;
    ensure!(pending["status"] == "waiting_for_input");
    ensure!(
        cli.run(&["thread", "cancel", id, "--turn", cancelled], true)
            .await?["status"]
            == "cancelled"
    );
    ensure!(
        cli.run(&["thread", "resume", id, "--turn", cancelled], false)
            .await?["status"]
            == "cancelled"
    );
    ensure!(fs::read_to_string(cli.work.join("note"))? == "externally edited");
    ensure!(model.finish()?.len() == 6);
    Ok(())
}

/// Opt-in inference against an operator-selected gateway and model. No fixture
/// response is installed. Replays go through fresh BRO processes on both turns.
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MODEL_ENDPOINT and BITROUTER_TEST_MODEL_NAME"]
async fn real_model_remembers_the_first_turn_after_a_cold_restart() -> Result<()> {
    let endpoint = std::env::var("BITROUTER_TEST_MODEL_ENDPOINT")?;
    let model = std::env::var("BITROUTER_TEST_MODEL_NAME")?;
    let mut cli = Consumer::new(&endpoint)?;
    cli.key = std::env::var_os("BITROUTER_API_KEY");
    let created = cli
        .run(
            &[
                "thread",
                "create",
                "--read-only",
                "--model",
                &model,
                "--max-output-tokens",
                "96",
            ],
            true,
        )
        .await?;
    let id = field(&created, "thread_id")?;
    let word = "violet";
    for (i, prompt) in [
        "Remember the color violet. Reply with just that color. No tools are needed.",
        "What color did I ask you to remember? Reply with just the color. No tools are needed.",
    ]
    .iter()
    .enumerate()
    {
        let key = i.to_string();
        let turn = cli
            .run(&["thread", "start", id, prompt, "--request-id", &key], true)
            .await?;
        ensure!(
            turn["status"] == "completed"
                && field(&turn, "final_answer")?.to_lowercase().contains(word),
            "unexpected model conclusion: {}",
            turn["final_answer"]
        );
        let before = cli.run(&["thread", "history", id], true).await?;
        let steps = before["turns"][i]["steps"]
            .as_array()
            .context("model receipts")?;
        ensure!(
            steps.len() == 1
                && steps[0]["state"]["record"] == "model_response"
                && !field(&steps[0]["state"]["response"], "completion_id")?.is_empty()
        );
        ensure!(
            cli.run(
                &["thread", "resume", id, "--turn", field(&turn, "turn_id")?],
                true
            )
            .await?
                == turn
        );
        ensure!(
            cli.run(&["thread", "start", id, prompt, "--request-id", &key], true)
                .await?["turn_id"]
                == turn["turn_id"]
        );
        ensure!(cli.run(&["thread", "history", id], true).await? == before);
    }
    let child = uuid::Uuid::new_v4().to_string();
    cli.run(&["agent", "create", id, &child], true).await?;
    let delegated = cli
        .run(
            &[
                "agent",
                "delegate",
                id,
                &child,
                "--request-id",
                "child",
                "Reply with only the color violet. No tools are needed.",
            ],
            true,
        )
        .await?;
    let turn = field(&delegated, "turn_id")?;
    let wake = ["agent", "wake", id, &child, turn];
    let completed = cli.run(&wake, true).await?;
    ensure!(
        completed["status"] == "completed"
            && field(&completed, "final_answer")?
                .to_lowercase()
                .contains(word)
    );
    ensure!(
        cli.run(&["agent", "wait", id, &child, turn], true).await?["outcome"]["final_answer"]
            == completed["final_answer"]
    );
    ensure!(cli.run(&wake, true).await? == completed);
    Ok(())
}

#[tokio::test]
async fn the_tree_shares_one_model_attempt_budget_across_all_workers() -> Result<()> {
    let model = Model::start(|messages| {
        let prompt = user(messages)?;
        if prompt == "split the work" {
            ensure!(
                tool_messages(messages).next().is_none(),
                "root sent a model request beyond the tree budget"
            );
            return Ok(calls(
                (0..4).map(|n| spawn(format!("budget-{n}"))).collect(),
            ));
        }
        ensure!(prompt.starts_with("budget-"));
        let n = tool_messages(messages).count();
        if n < 31 {
            Ok(calls(vec![read(format!("read-{n}"), "seed")]))
        } else {
            Ok(answer("finished"))
        }
    })?;
    let cli = Consumer::new(&model.endpoint)?;
    fs::write(cli.work.join("seed"), "budget evidence")?;
    let terminal = cli
        .run(
            &["task", "run", "--managed", "--read-only", "split the work"],
            false,
        )
        .await?;
    ensure!(
        terminal["status"] == "failed" && terminal["detail"] == "tree model attempt limit exceeded"
    );
    let id = field(&terminal, "thread_id")?;
    let turn = field(&terminal, "turn_id")?;
    let history = cli.run(&["thread", "history", id], true).await?;
    let tree = &history["turns"][0];
    let steps = tree["steps"].as_array().context("root steps")?.len()
        + tree["children"]
            .as_object()
            .context("workers")?
            .values()
            .map(|c| {
                c["turns"][0]["steps"]
                    .as_array()
                    .context("child steps")
                    .map(Vec::len)
            })
            .collect::<Result<Vec<_>>>()?
            .iter()
            .sum::<usize>();
    ensure!(steps == 128);
    ensure!(
        cli.run(&["thread", "resume", id, "--turn", turn], false)
            .await?
            == terminal
    );
    ensure!(model.finish()?.len() == 128);
    Ok(())
}
