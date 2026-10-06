use std::time::Duration;

use bitrouter_sdk::language_model::{Tool, ToolResultOutput};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::read::read_page;
#[cfg(unix)]
use super::read::{directory_name, sort_directory_entries};
use super::*;
use crate::agent::{RunEvent, ToolMode};
use crate::store::EffectStatus;

fn success(output: ToolResultOutput) -> Result<ToolResultOutput, String> {
    if let ToolResultOutput::ErrorJson { value } = &output {
        return Err(value.to_string());
    }
    Ok(output)
}

fn text_output(output: ToolResultOutput) -> Result<String, Box<dyn std::error::Error>> {
    match success(output)? {
        ToolResultOutput::Text { value } => Ok(value),
        _ => Err("expected text output".into()),
    }
}

#[tokio::test]
async fn registry_profiles_reject_legacy_names_and_invalid_arguments()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    for mode in [ToolMode::ReadOnly, ToolMode::Coding] {
        let tools = WorkspaceTools::new(workspace.path(), mode)?;
        let declarations = tools.declarations();
        let names: Vec<_> = declarations
            .iter()
            .filter_map(|tool| match tool {
                Tool::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            if mode == ToolMode::Coding {
                vec!["read", "glob", "grep", "write", "edit", "shell"]
            } else {
                vec!["read", "glob", "grep"]
            }
        );
        for name in ["ls", "find", "bash", "powershell"] {
            assert!(!WorkspaceTools::allowed(mode, name));
            assert!(WorkspaceTools::validate(name, "{}").is_err());
            let (output, effect) = tools
                .execute_with_effect(name, "{}", &CancellationToken::new(), "legacy", None)
                .await;
            assert!(output.is_error());
            assert_eq!(effect, EffectStatus::NotExecuted);
        }
        if mode == ToolMode::ReadOnly {
            assert!(tools.interpreter.is_none());
            assert!(
                tools
                    .execute(
                        "write",
                        r#"{"path":"created","content":"bad"}"#,
                        &CancellationToken::new(),
                        "forged",
                        None
                    )
                    .await
                    .is_error()
            );
            assert!(!workspace.path().join("created").exists());
        } else {
            let shell = tools.interpreter.as_ref().ok_or("missing interpreter")?;
            let Tool::Function {
                description: Some(description),
                ..
            } = &declarations[5]
            else {
                return Err("shell description missing".into());
            };
            assert!(description.contains(&shell.executable.display().to_string()));
        }
    }
    for args in [
        r#"{"path":".","offset":0}"#,
        r#"{"path":".","limit":0}"#,
        r#"{"path":".","limit":2001}"#,
        r#"{"path":".","offset":-1}"#,
        r#"{"path":".","offset":1.5}"#,
        r#"{"path":".","offset":18446744073709551616}"#,
        r#"{"path":".","mode":"directory"}"#,
        "{}",
        r#"{"path":""}"#,
    ] {
        assert!(WorkspaceTools::validate("read", args).is_err(), "{args}");
    }
    for args in [
        r#"{"command":" "}"#,
        r#"{"command":"echo hi","timeout":0}"#,
        r#"{"command":"echo hi","timeout":121}"#,
        r#"{"command":"echo hi","interpreter":"bash"}"#,
    ] {
        assert!(WorkspaceTools::validate("shell", args).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn read_directory_pages_include_ignored_entries_and_escape_names()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::create_dir(workspace.path().join(".git"))?;
    fs::write(workspace.path().join(".gitignore"), "ignored.txt\n")?;
    fs::write(workspace.path().join("ignored.txt"), "hidden from search")?;
    fs::write(workspace.path().join("src/note.txt"), "one\ntwo\nthree\n")?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
    let cancel = CancellationToken::new();
    let first = text_output(
        tools
            .execute("read", r#"{"path":".","limit":2}"#, &cancel, "page1", None)
            .await,
    )?;
    assert_eq!(
        first,
        "Directory \".\"\nE1: directory \".git/\"\nE2: file \".gitignore\"\n[output truncated; continue with offset=3]\n"
    );
    let second = text_output(
        tools
            .execute(
                "read",
                r#"{"path":".","offset":3,"limit":2}"#,
                &cancel,
                "page2",
                None,
            )
            .await,
    )?;
    assert_eq!(
        second,
        "Directory \".\"\nE3: file \"ignored.txt\"\nE4: directory \"src/\"\n"
    );
    let file = text_output(
        tools
            .execute(
                "read",
                r#"{"path":"src/note.txt","offset":2,"limit":1}"#,
                &cancel,
                "file",
                None,
            )
            .await,
    )?;
    assert_eq!(
        file,
        "File \"src/note.txt\"\nL2: two\n[output truncated; continue with offset=3]\n"
    );
    for path in [".", "src/note.txt"] {
        let result = text_output(
            tools
                .execute(
                    "read",
                    &serde_json::json!({"path":path,"offset":100}).to_string(),
                    &cancel,
                    "end",
                    None,
                )
                .await,
        )?;
        assert!(result.ends_with("(end of input)\n"));
        assert!(!result.contains("truncated"));
    }
    fs::create_dir(workspace.path().join("empty"))?;
    fs::write(workspace.path().join("empty.txt"), "")?;
    for (path, message) in [
        ("empty", "(empty directory)"),
        ("empty.txt", "(empty file)"),
    ] {
        assert!(
            text_output(
                tools
                    .execute(
                        "read",
                        &serde_json::json!({"path":path}).to_string(),
                        &cancel,
                        "empty",
                        None
                    )
                    .await
            )?
            .contains(message)
        );
    }
    let order = text_output(read_page(
        "Directory \"test\"\n".into(),
        vec!["E1: first\n".into(), "E2: second\n".into()].into_iter(),
        1,
        1,
        "empty",
    )?)?;
    assert!(order.ends_with("offset=2]\n"));
    Ok(())
}

#[tokio::test]
async fn read_byte_pages_are_bounded_and_large_records_fail()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    fs::write(
        workspace.path().join("pages.txt"),
        format!("{}\n{}\n", "α".repeat(15000), "β".repeat(15000)),
    )?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
    let cancel = CancellationToken::new();
    let first = text_output(
        tools
            .execute("read", r#"{"path":"pages.txt"}"#, &cancel, "first", None)
            .await,
    )?;
    assert!(first.len() <= MAX_READ_BYTES);
    assert!(first.contains("offset=2]"));
    assert!(!first.contains('β'));
    let second = text_output(
        tools
            .execute(
                "read",
                r#"{"path":"pages.txt","offset":2}"#,
                &cancel,
                "second",
                None,
            )
            .await,
    )?;
    assert!(second.len() <= MAX_READ_BYTES);
    assert!(second.contains('β'));
    assert!(!second.contains("truncated"));
    fs::write(
        workspace.path().join("huge.txt"),
        "x".repeat(MAX_READ_BYTES),
    )?;
    assert!(
        tools
            .execute("read", r#"{"path":"huge.txt"}"#, &cancel, "huge", None)
            .await
            .is_error()
    );
    fs::write(workspace.path().join("binary"), [0xff, 0xfe])?;
    assert!(
        tools
            .execute("read", r#"{"path":"binary"}"#, &cancel, "binary", None)
            .await
            .is_error()
    );
    for path in ["../outside", "/", "missing"] {
        assert!(
            tools
                .execute(
                    "read",
                    &serde_json::json!({"path":path}).to_string(),
                    &cancel,
                    "invalid",
                    None
                )
                .await
                .is_error()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_case_ties_symlinks_special_files_and_non_utf8_names()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStringExt;
    let workspace = TempDir::new()?;
    let outside = TempDir::new()?;
    fs::write(workspace.path().join("A"), "inside")?;
    fs::write(workspace.path().join("a"), "inside")?;
    // Some macOS filesystems are case-insensitive. Test the sort rule directly
    // as well as inspecting actual directory entries.
    let mut entries = [
        ("a".into(), "file"),
        ("A".into(), "file"),
        ("b".into(), "file"),
    ];
    sort_directory_entries(&mut entries);
    assert_eq!(entries.map(|(name, _)| name), ["A", "a", "b"]);
    fs::write(workspace.path().join("new\nline"), "escaped")?;
    fs::write(outside.path().join("secret"), "outside")?;
    std::os::unix::fs::symlink(
        workspace.path().join("A"),
        workspace.path().join("inside-link"),
    )?;
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link"))?;
    let socket = std::os::unix::net::UnixListener::bind(workspace.path().join("socket"))?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
    let cancel = CancellationToken::new();
    let result = text_output(
        tools
            .execute("read", r#"{"path":"."}"#, &cancel, "root", None)
            .await,
    )?;
    assert!(result.contains("file \"new\\nline\""));
    assert!(result.contains("symlink \"outside-link\""));
    assert!(!result.contains("outside\n"));
    assert!(
        text_output(
            tools
                .execute("read", r#"{"path":"inside-link"}"#, &cancel, "link", None)
                .await
        )?
        .contains("inside")
    );
    for path in ["outside-link", "socket"] {
        assert!(
            tools
                .execute(
                    "read",
                    &serde_json::json!({"path":path}).to_string(),
                    &cancel,
                    "rejected",
                    None
                )
                .await
                .is_error()
        );
    }
    drop(socket);
    assert!(directory_name(std::ffi::OsString::from_vec(vec![0xff])).is_err());
    Ok(())
}

#[tokio::test]
async fn glob_paths_directories_limits_and_cancellation_keep_search_semantics()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    fs::create_dir_all(workspace.path().join("src/nested"))?;
    fs::write(workspace.path().join("src/main.rs"), "root")?;
    fs::write(workspace.path().join("src/nested/lib.rs"), "nested")?;
    fs::write(workspace.path().join(".visible.rs"), "hidden")?;
    fs::create_dir(workspace.path().join(".git"))?;
    fs::write(workspace.path().join(".git/private.rs"), "exclude")?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
    let cancel = CancellationToken::new();
    let paths = text_output(
        tools
            .execute("glob", r#"{"pattern":"*.rs"}"#, &cancel, "all", None)
            .await,
    )?;
    for expected in ["src/main.rs", "src/nested/lib.rs", ".visible.rs"] {
        assert!(paths.contains(expected));
    }
    assert!(!paths.contains("private.rs"));
    assert_eq!(
        text_output(
            tools
                .execute(
                    "glob",
                    r#"{"pattern":"nested/*.rs","path":"src"}"#,
                    &cancel,
                    "relative",
                    None
                )
                .await
        )?,
        "src/nested/lib.rs\n"
    );
    assert_eq!(
        text_output(
            tools
                .execute(
                    "glob",
                    r#"{"pattern":"nested","path":"src"}"#,
                    &cancel,
                    "dir",
                    None
                )
                .await
        )?,
        "src/nested/\n"
    );
    let limited = text_output(
        tools
            .execute(
                "glob",
                r#"{"pattern":"*.rs","limit":1}"#,
                &cancel,
                "limit",
                None,
            )
            .await,
    )?;
    assert!(limited.contains("results truncated"));
    cancel.cancel();
    assert!(
        tools
            .execute("glob", r#"{"pattern":"*.rs"}"#, &cancel, "cancelled", None)
            .await
            .is_error()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn interpreter_availability_fallback_is_declared_and_never_retried()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let workspace = TempDir::new()?;
    let bins = TempDir::new()?;
    let candidates = [("bash", "bash"), ("sh", "sh")];
    assert!(Interpreter::search(None, &candidates).is_err());
    assert!(Interpreter::search(Some(bins.path().as_os_str()), &candidates).is_err());
    let sh = bins.path().join("sh");
    fs::write(&sh, "#!/bin/sh\nprintf fallback > unexpected\n")?;
    fs::set_permissions(&sh, fs::Permissions::from_mode(0o755))?;
    let fallback = Interpreter::search(Some(bins.path().as_os_str()), &candidates)?;
    assert_eq!(fallback.dialect, "sh");
    let bash = bins.path().join("bash");
    fs::write(&bash, "#!/bin/sh\nprintf selected\n")?;
    fs::set_permissions(&bash, fs::Permissions::from_mode(0o755))?;
    let preferred = Interpreter::search(Some(bins.path().as_os_str()), &candidates)?;
    assert_eq!(preferred.dialect, "bash");
    let tools = WorkspaceTools {
        root: workspace.path().canonicalize()?,
        mode: ToolMode::Coding,
        interpreter: Some(preferred),
        read_gate: None,
    };
    let value = success(
        tools
            .execute(
                "shell",
                r#"{"command":"echo hi"}"#,
                &CancellationToken::new(),
                "selected",
                None,
            )
            .await,
    )?;
    assert!(
        matches!(value, ToolResultOutput::Json { value } if value["interpreter"]["dialect"] == "bash" && value["stdout"] == "selected")
    );
    fs::remove_file(bash)?;
    let (output, effect) = tools
        .execute_with_effect(
            "shell",
            r#"{"command":"echo hi"}"#,
            &CancellationToken::new(),
            "lost",
            None,
        )
        .await;
    assert!(output.is_error());
    assert_eq!(effect, EffectStatus::NotExecuted);
    assert!(!workspace.path().join("unexpected").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shell_nonzero_exit_and_output_caps_remain_explicit()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let result = success(
        tools
            .execute(
                "shell",
                r#"{"command":"printf out; printf err >&2; exit 7"}"#,
                &CancellationToken::new(),
                "exit",
                None,
            )
            .await,
    )?;
    assert!(
        matches!(result, ToolResultOutput::Json { value } if value["exit_status"] == 7 && value["stdout"] == "out" && value["stderr"] == "err" && value["timed_out"] == false)
    );
    let result = success(
        tools
            .execute(
                "shell",
                r#"{"command":"head -c 40000 /dev/zero | tr '\\0' x"}"#,
                &CancellationToken::new(),
                "cap",
                None,
            )
            .await,
    )?;
    let ToolResultOutput::Json { value } = result else {
        return Err("expected shell output".into());
    };
    assert_eq!(
        value["stdout"].as_str().ok_or("stdout")?.len(),
        MAX_OUTPUT_BYTES
    );
    assert_eq!(value["stdout_truncated"], true);
    Ok(())
}

#[cfg(unix)]
async fn assert_unix_descendant_cleanup(action: &str) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let (sender, mut receiver) = mpsc::channel(64);
    let args = serde_json::json!({"command": "while true; do echo tick >>ticks.txt; sleep 0.05; done & while [ ! -s ticks.txt ]; do sleep 0.01; done; printf ready; wait", "timeout": if action == "timeout" { 1 } else { 30 }}).to_string();
    let task = tokio::spawn(async move {
        tools
            .execute("shell", &args, &task_cancel, "cleanup", Some(&sender))
            .await
    });
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = receiver.recv().await {
            if matches!(event, RunEvent::ToolOutputDelta { text, .. } if text.contains("ready")) {
                return Ok::<(), String>(());
            }
        }
        Err("no readiness output".into())
    })
    .await;
    if !matches!(ready, Ok(Ok(()))) {
        cancel.cancel();
        task.await?;
        return Err("descendant failed to start".into());
    }
    match action {
        "cancel" => cancel.cancel(),
        "drop" => task.abort(),
        _ => {}
    }
    let result = tokio::time::timeout(Duration::from_secs(5), task).await?;
    match action {
        "cancel" => assert!(result?.is_error()),
        "drop" => assert!(result.is_err_and(|error| error.is_cancelled())),
        "timeout" => assert!(
            matches!(success(result?)?, ToolResultOutput::Json { value } if value["timed_out"] == true)
        ),
        _ => return Err("unknown action".into()),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ticks = fs::read(workspace.path().join("ticks.txt"))?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(ticks, fs::read(workspace.path().join("ticks.txt"))?);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shell_cancellation_stops_unix_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_unix_descendant_cleanup("cancel").await
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_stops_unix_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_unix_descendant_cleanup("timeout").await
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_shell_future_stops_unix_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_unix_descendant_cleanup("drop").await
}

#[test]
fn openai_requests_preserve_optional_tool_arguments() -> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_sdk::language_model::protocol::{
        OutboundAdapter, chat_completions::ChatCompletionsAdapter, responses::ResponsesAdapter,
    };

    for mode in [ToolMode::Coding, ToolMode::ReadOnly] {
        let prompt = crate::context::build(
            "fixture",
            None,
            "inspect",
            &[],
            WorkspaceTools::new(TempDir::new()?.path(), mode)?.declarations(),
            512 * 1024,
        )?;
        for adapter in [
            &ChatCompletionsAdapter as &dyn OutboundAdapter,
            &ResponsesAdapter,
        ] {
            let request = adapter.render_request(&prompt)?;
            for tool in request["tools"].as_array().ok_or("missing tools")? {
                let function = tool.get("function").unwrap_or(tool);
                assert_eq!(function["strict"], false);
                let name = function["name"].as_str().ok_or("missing tool name")?;
                if name == "read" {
                    assert_eq!(
                        function["parameters"]["required"],
                        serde_json::json!(["path"])
                    );
                    assert!(function["parameters"]["properties"].get("offset").is_some());
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn write_creates_parents_and_edit_applies_original_ranges()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    success(
        tools
            .execute(
                "write",
                r#"{"path":"src/note.txt","content":"\uFEFFone\r\ntwo\r\nthree\r\n"}"#,
                &cancel,
                "write-1",
                None,
            )
            .await,
    )?;
    let path = workspace.path().join("src/note.txt");
    success(tools.execute(
        "edit",
        r#"{"path":"src/note.txt","edits":[{"oldText":"three","newText":"THREE"},{"oldText":"one\ntwo","newText":"ONE\nTWO"}]}"#,
        &cancel, "edit-1", None,
    ).await)?;
    assert_eq!(fs::read_to_string(path)?, "\u{feff}ONE\r\nTWO\r\nTHREE\r\n");
    Ok(())
}

#[tokio::test]
async fn ambiguous_or_overlapping_edits_leave_file_intact() -> Result<(), Box<dyn std::error::Error>>
{
    let workspace = TempDir::new()?;
    let path = workspace.path().join("note.txt");
    fs::write(&path, "abc abc\n")?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    for edits in [
        serde_json::json!([{"oldText": "abc", "newText": "x"}]),
        serde_json::json!([
            {"oldText": "abc abc", "newText": "x"},
            {"oldText": "abc ", "newText": "y"}
        ]),
        serde_json::json!([
            {"oldText": "abc abc", "newText": "x"},
            {"oldText": "missing", "newText": "y"}
        ]),
    ] {
        let args = serde_json::json!({"path": "note.txt", "edits": edits}).to_string();
        assert!(
            tools
                .execute("edit", &args, &cancel, "edit", None)
                .await
                .is_error()
        );
        assert_eq!(fs::read_to_string(&path)?, "abc abc\n");
    }
    Ok(())
}

#[tokio::test]
async fn inspection_tools_respect_ignore_limits_and_workspace_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    fs::create_dir(workspace.path().join("src"))?;
    fs::write(workspace.path().join(".gitignore"), "ignored.txt\n")?;
    fs::write(workspace.path().join("src/main.rs"), "alpha\nbeta alpha\n")?;
    fs::write(workspace.path().join("ignored.txt"), "alpha\n")?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    let ls = success(
        tools
            .execute("read", r#"{"path":"."}"#, &cancel, "ls", None)
            .await,
    )?;
    assert!(
        matches!(ls, ToolResultOutput::Text { value } if value.contains(".gitignore") && value.contains("src/"))
    );
    let find = success(
        tools
            .execute("glob", r#"{"pattern":"*.rs"}"#, &cancel, "glob", None)
            .await,
    )?;
    assert!(matches!(find, ToolResultOutput::Text { value } if value == "src/main.rs\n"));
    let grep = success(
        tools
            .execute(
                "grep",
                r#"{"pattern":"alpha","glob":"*.rs"}"#,
                &cancel,
                "grep",
                None,
            )
            .await,
    )?;
    assert!(
        matches!(grep, ToolResultOutput::Text { value } if value == "src/main.rs:1: alpha\nsrc/main.rs:2: beta alpha\n")
    );
    let ignored = success(
        tools
            .execute(
                "grep",
                r#"{"pattern":"alpha","glob":"*.txt"}"#,
                &cancel,
                "grep",
                None,
            )
            .await,
    )?;
    assert!(matches!(ignored, ToolResultOutput::Text { value } if value == "No matches found"));
    let capped = success(
        tools
            .execute(
                "grep",
                r#"{"pattern":"alpha","limit":1}"#,
                &cancel,
                "grep",
                None,
            )
            .await,
    )?;
    assert!(
        matches!(capped, ToolResultOutput::Text { value } if value.contains("[matches truncated") && !value.contains("src/main.rs:2:"))
    );
    assert!(
        tools
            .execute("read", r#"{"path":"../"}"#, &cancel, "escape", None)
            .await
            .is_error()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn search_does_not_follow_symlinks_outside_workspace()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let outside = TempDir::new()?;
    fs::write(outside.path().join("secret.txt"), "do not return this\n")?;
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("link"))?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    assert!(
        tools
            .execute(
                "grep",
                r#"{"pattern":"secret","path":"link"}"#,
                &cancel,
                "grep",
                None
            )
            .await
            .is_error()
    );
    let find = success(
        tools
            .execute("glob", r#"{"pattern":"*.txt"}"#, &cancel, "glob", None)
            .await,
    )?;
    assert!(
        matches!(find, ToolResultOutput::Text { value } if value == "No files found matching pattern")
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn write_rejects_parent_symlink_escape() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let outside = TempDir::new()?;
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("link"))?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let output = tools
        .execute(
            "write",
            r#"{"path":"link/nested/file.txt","content":"escape"}"#,
            &CancellationToken::new(),
            "write",
            None,
        )
        .await;
    assert!(output.is_error());
    assert!(!outside.path().join("nested").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shell_emits_output_before_exit() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let (sender, mut receiver) = mpsc::channel(64);
    let command = tokio::spawn(async move {
        tools
            .execute(
                "shell",
                r#"{"command":"printf first; sleep 0.2; printf second"}"#,
                &CancellationToken::new(),
                "bash-1",
                Some(&sender),
            )
            .await
    });
    let first = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await?
        .ok_or("missing live output")?;
    assert!(matches!(first, RunEvent::ToolOutputDelta { text, .. } if text.contains("first")));
    assert!(!command.is_finished());
    let result = success(command.await?)?;
    assert!(matches!(result, ToolResultOutput::Json { value } if value["stdout"] == "firstsecond"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shell_exit_stops_background_descendants() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let result = tokio::time::timeout(Duration::from_secs(5), tools.execute("shell",
        r#"{"command":"while true; do echo tick >>ticks.txt; sleep 0.05; done & while [ ! -s ticks.txt ]; do sleep 0.01; done"}"#,
        &CancellationToken::new(), "shell", None)).await?;
    success(result)?;
    let ticks = std::fs::read(workspace.path().join("ticks.txt"))?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(ticks, std::fs::read(workspace.path().join("ticks.txt"))?);
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_nonzero_exit_output_caps_and_streaming()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    let result = success(
        tools
            .execute(
                "shell",
                r#"{"command":"[Console]::Write('out'); [Console]::Error.Write('err'); exit 7"}"#,
                &cancel,
                "exit",
                None,
            )
            .await,
    )?;
    assert!(
        matches!(result, ToolResultOutput::Json { value } if value["exit_status"] == 7 && value["stdout"] == "out" && value["stderr"] == "err" && value["timed_out"] == false && value["interpreter"]["dialect"] == "powershell")
    );
    let result = success(
        tools
            .execute(
                "shell",
                r#"{"command":"[Console]::Write(('x' * 40000)); [Console]::Error.Write(('y' * 40000))"}"#,
                &cancel,
                "cap",
                None,
            )
            .await,
    )?;
    let ToolResultOutput::Json { value } = result else {
        return Err("expected shell output".into());
    };
    for stream in ["stdout", "stderr"] {
        assert_eq!(
            value[stream].as_str().ok_or("missing stream")?.len(),
            MAX_OUTPUT_BYTES
        );
        assert_eq!(value[format!("{stream}_truncated")], true);
    }
    let (sender, mut receiver) = mpsc::channel(64);
    let command = tokio::spawn(async move {
        tools
            .execute(
                "shell",
                r#"{"command":"[Console]::Write('first'); Start-Sleep -Seconds 2; [Console]::Write('second')"}"#,
                &CancellationToken::new(),
                "stream",
                Some(&sender),
            )
            .await
    });
    let first = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await?
        .ok_or("missing live output")?;
    assert!(matches!(first, RunEvent::ToolOutputDelta { text, .. } if text.contains("first")));
    assert!(!command.is_finished());
    assert!(
        matches!(success(command.await?)?, ToolResultOutput::Json { value } if value["stdout"] == "firstsecond")
    );
    Ok(())
}

#[cfg(windows)]
async fn assert_windows_descendant_cleanup(action: &str) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(
        workspace.path().join("child.ps1"),
        "Add-Content ticks.txt tick\n[Console]::WriteLine('ready')\nwhile ($true) { Add-Content ticks.txt tick; Start-Sleep -Milliseconds 50 }",
    )?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let (sender, mut receiver) = mpsc::channel(64);
    let command = if action == "exit" {
        // The shell exits normally while its background child owns a file.
        "Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -File child.ps1' -NoNewWindow; while (!(Test-Path ticks.txt)) { Start-Sleep -Milliseconds 20 }; [Console]::WriteLine('ready')"
    } else {
        "& powershell.exe -NoProfile -NonInteractive -File child.ps1"
    };
    let timeout = if action == "timeout" { 3 } else { 30 };
    let task = tokio::spawn(async move {
        tools
            .execute(
                "shell",
                &serde_json::json!({"command":command, "timeout":timeout}).to_string(),
                &task_cancel,
                "shell",
                Some(&sender),
            )
            .await
    });
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = receiver.recv().await {
            if matches!(event, RunEvent::ToolOutputDelta {text, ..} if text.contains("ready")) {
                return Ok::<(), String>(());
            }
        }
        Err("shell exited before descendant started".to_string())
    })
    .await;
    if !matches!(ready, Ok(Ok(()))) {
        cancel.cancel();
        task.await?;
        return Err("descendant did not start".into());
    }
    match action {
        "cancel" => cancel.cancel(),
        "drop" => task.abort(),
        _ => {}
    }
    let result = tokio::time::timeout(Duration::from_secs(10), task).await?;
    match action {
        "cancel" => assert!(result?.is_error()),
        "drop" => assert!(result.is_err_and(|error| error.is_cancelled())),
        "timeout" => assert!(
            matches!(success(result?)?, ToolResultOutput::Json {value} if value["timed_out"] == true)
        ),
        "exit" => {
            success(result?)?;
        }
        _ => return Err("unknown cleanup action".into()),
    }
    let ticks = std::fs::read(workspace.path().join("ticks.txt"))?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(ticks, std::fs::read(workspace.path().join("ticks.txt"))?);
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_cancellation_stops_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_windows_descendant_cleanup("cancel").await
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_timeout_stops_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_windows_descendant_cleanup("timeout").await
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_exit_stops_background_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_windows_descendant_cleanup("exit").await
}

#[cfg(windows)]
#[tokio::test]
async fn dropped_powershell_future_stops_descendants() -> Result<(), Box<dyn std::error::Error>> {
    assert_windows_descendant_cleanup("drop").await
}

#[tokio::test]
async fn rejected_effectful_tools_report_no_mutation() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
    fs::write(workspace.path().join("note.txt"), "original original")?;
    let cancel = CancellationToken::new();
    let cases = [
        (
            "edit",
            serde_json::json!({"path":"note.txt","edits":[{"oldText":"absent","newText":"changed"}]}),
        ),
        (
            "edit",
            serde_json::json!({"path":"note.txt","edits":[{"oldText":"original","newText":"changed"}]}),
        ),
        (
            "edit",
            serde_json::json!({"path":"missing.txt","edits":[{"oldText":"old","newText":"new"}]}),
        ),
        (
            "write",
            serde_json::json!({"path":"../escape.txt","content":"new"}),
        ),
        ("shell", serde_json::json!({"command":""})),
    ];
    for (name, arguments) in cases {
        let (output, effect) = tools
            .execute_with_effect(name, &arguments.to_string(), &cancel, "rejected", None)
            .await;
        assert!(output.is_error());
        assert_eq!(effect, EffectStatus::NotExecuted);
    }
    assert_eq!(
        fs::read_to_string(workspace.path().join("note.txt"))?,
        "original original"
    );
    assert_eq!(fs::read_dir(workspace.path())?.count(), 1);
    let (_, effect) = tools
        .execute_with_effect(
            "write",
            r#"{"path":"new/note.txt","content":"written"}"#,
            &cancel,
            "written",
            None,
        )
        .await;
    assert_eq!(effect, EffectStatus::Completed);
    Ok(())
}

#[test]
fn persistence_failure_after_temporary_write_retains_uncertainty()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let path = workspace.path().join("note.txt");
    fs::write(&path, "newer content")?;
    let mut effect = EffectStatus::NotExecuted;
    assert!(persist_text(&path, b"replacement", Some(b"stale content"), &mut effect).is_err());
    assert_eq!(effect, EffectStatus::Unknown);
    assert_eq!(fs::read_to_string(&path)?, "newer content");
    Ok(())
}
