//! Startup instructions cross native context, permission and durability boundaries.

use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{Content, Prompt, Role, ToolResultOutput};
use serde_json::json;
use tempfile::TempDir;

use super::support::*;
use crate::harness::{HarnessConfig, instructions::InstructionConfig};
use crate::service::ThreadService;
use crate::store::{ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::thread::{PermissionProfile, WorkspaceGrant};
use crate::turn::TurnStatus;

fn startup(prompt: &Prompt) -> Option<&str> {
    prompt
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text, .. } if text.starts_with("# AGENTS.md instructions for ") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .next_back()
}

#[tokio::test]
async fn agents_md_is_durable_user_context_in_both_tool_modes()
-> Result<(), Box<dyn std::error::Error>> {
    for read_only in [false, true] {
        let workspace = TempDir::new()?;
        let body = "# Project conventions\n\nPreserve 中文 and arbitrary Markdown.\n\n~~~sh\nprintf 'project check'\n~~~\n";
        std::fs::write(workspace.path().join("AGENTS.md"), body)?;
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn()])?,
            &[workspace.path().into()],
            store.clone(),
        )?;
        let mut request = thread_request(&workspace, "create");
        if read_only {
            request.config = request.config.read_only();
        } else {
            request.config.instructions = "HOST_BASE_INSTRUCTIONS".into();
        }
        let base = request.config.instructions.clone();
        let created = service
            .create_thread(&service.inner.instance_id, request)
            .await?;
        let view = service.read_thread_view(&target(&created), &CallerContext::local())?;
        assert_eq!(view.config.instructions, base);
        assert!(
            service
                .lock_state()
                .threads
                .get(&created.thread_id)
                .is_some_and(|thread| thread.instructions.is_none())
        );
        let accepted = service
            .start_turn(
                &target(&created),
                &CallerContext::local(),
                input("inspect", "turn"),
            )
            .await?;
        let done = wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
        assert_eq!(done.status, TurnStatus::Completed);
        let requests = prompts(store.as_ref(), &created.thread_id, &accepted.turn_id).await?;
        let system = requests[0].system.as_deref().ok_or("system prompt")?;
        assert!(system.starts_with(&base));
        assert!(system.contains("more deeply nested"));
        assert!(!system.contains("Project conventions"));
        assert!(startup(&requests[0]).is_some_and(|text| text.contains(body)));
        assert_eq!(requests[0].messages[0].role, Role::User);
        assert_eq!(requests[0].messages.len(), 2);
        if read_only {
            assert_eq!(requests[0].tools.len(), 3);
        }
        let inventory = done.resources.as_ref().ok_or("inventory")?;
        assert!(
            !inventory
                .instructions
                .iter()
                .any(|material| material.provenance.contains("AGENTS.md"))
        );
        let saved = store.load(&created.thread_id).await?.ok_or("journal")?;
        let facts: Vec<_> = saved.records.iter().map(turn_fact).collect();
        let position = facts
            .iter()
            .position(|fact| {
                matches!(fact, ExecutionRecord::InstructionContext { snapshot, .. }
                if snapshot.materials.iter().any(|material|
                    material.provenance.contains("AGENTS.md") && material.sha256.len() == 64))
            })
            .ok_or("instruction snapshot")?;
        let model = facts
            .iter()
            .position(|fact| matches!(fact, ExecutionRecord::ModelRequest { .. }))
            .ok_or("model request")?;
        assert!(position < model);
        assert!(!saved.records.iter().any(|record| matches!(
            record,
            ExecutionRecord::ThreadEvent { event }
                if serde_json::to_string(&event.changes).is_ok_and(|text| text.contains("Project conventions"))
        )));
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn absent_or_empty_startup_files_do_not_load_descendants()
-> Result<(), Box<dyn std::error::Error>> {
    for body in [None, Some(""), Some(" \n\t")] {
        let workspace = TempDir::new()?;
        if let Some(body) = body {
            std::fs::write(workspace.path().join("AGENTS.md"), body)?;
        }
        std::fs::write(workspace.path().join("AGENT.md"), "UNSUPPORTED_ALIAS")?;
        std::fs::create_dir(workspace.path().join("nested"))?;
        std::fs::write(workspace.path().join("nested/AGENTS.md"), "NESTED_ONLY")?;
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn()])?,
            &[workspace.path().into()],
            store.clone(),
        )?;
        let mut request = request(&workspace);
        request.config.instructions = "HOST_BASE_INSTRUCTIONS".into();
        let accepted = service.submit_fixture(request).await?;
        assert_eq!(
            wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
                .await?
                .status,
            TurnStatus::Completed
        );
        let requests = prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
        assert!(
            requests[0]
                .system
                .as_deref()
                .is_some_and(|system| system.starts_with("HOST_BASE_INSTRUCTIONS"))
        );
        assert!(startup(&requests[0]).is_none());
        assert!(!serde_json::to_string(&requests[0].messages)?.contains("NESTED_ONLY"));
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn global_and_project_chain_use_override_fallback_and_root_to_cwd_order()
-> Result<(), Box<dyn std::error::Error>> {
    let project = TempDir::new()?;
    let global = TempDir::new()?;
    let cwd = project.path().join("packages/api");
    std::fs::create_dir_all(&cwd)?;
    std::fs::write(project.path().join(".git"), "gitdir: fixture")?;
    std::fs::write(project.path().join("AGENTS.md"), "ROOT_RULE")?;
    std::fs::write(project.path().join("packages/AGENTS.md"), "SHADOWED_PARENT")?;
    std::fs::write(
        project.path().join("packages/AGENTS.override.md"),
        "PARENT_OVERRIDE",
    )?;
    std::fs::write(cwd.join("TEAM.md"), "CWD_FALLBACK")?;
    std::fs::write(global.path().join("AGENTS.override.md"), "  \n")?;
    std::fs::write(global.path().join("AGENTS.md"), "GLOBAL_RULE")?;
    std::fs::create_dir(cwd.join("deeper"))?;
    std::fs::write(cwd.join("deeper/AGENTS.md"), "UNVISITED_RULE")?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[project.path().into(), cwd.clone()],
        store.clone(),
    )?
    .with_resources(HarnessConfig {
        instructions: InstructionConfig {
            global_root: Some(global.path().canonicalize()?),
            project_doc_fallback_filenames: vec!["TEAM.md".into()],
            ..Default::default()
        },
        ..Default::default()
    })?;
    let mut request = request(&project);
    request.workspace = cwd;
    let accepted = service.submit_fixture(request).await?;
    assert_eq!(
        wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let requests = prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
    let body = startup(&requests[0]).ok_or("startup context")?;
    let mut previous = 0;
    for token in [
        "GLOBAL_RULE",
        "ROOT_RULE",
        "PARENT_OVERRIDE",
        "CWD_FALLBACK",
    ] {
        let position = body.find(token).ok_or("missing chain entry")?;
        assert!(position >= previous);
        previous = position + token.len();
    }
    assert!(!body.contains("SHADOWED_PARENT"));
    assert!(!body.contains("UNVISITED_RULE"));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn startup_bytes_are_bounded_and_decode_like_codex() -> Result<(), Box<dyn std::error::Error>>
{
    for bytes in [Some(vec![0xff]), Some(vec![b'x'; 32 * 1024 + 1]), None] {
        let workspace = TempDir::new()?;
        if let Some(bytes) = &bytes {
            std::fs::write(workspace.path().join("AGENTS.md"), bytes)?;
        } else {
            std::fs::create_dir(workspace.path().join("AGENTS.md"))?;
        }
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn()])?,
            &[workspace.path().into()],
            store.clone(),
        )?;
        let accepted = service.submit_fixture(request(&workspace)).await?;
        assert_eq!(
            wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
                .await?
                .status,
            TurnStatus::Completed
        );
        let saved = store.load(&accepted.thread_id).await?.ok_or("journal")?;
        let snapshot = saved
            .records
            .iter()
            .find_map(|record| match turn_fact(record) {
                ExecutionRecord::InstructionContext { snapshot, .. } => Some(snapshot),
                _ => None,
            })
            .ok_or("snapshot")?;
        match bytes {
            Some(bytes) if bytes.len() > 32 * 1024 => {
                assert_eq!(snapshot.body.len(), 32 * 1024);
                assert_eq!(snapshot.warnings.len(), 1);
            }
            Some(_) => assert_eq!(snapshot.body, "\u{fffd}"),
            None => assert!(snapshot.body.is_empty()),
        }
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn project_chain_respects_git_read_ceiling_and_shared_budget()
-> Result<(), Box<dyn std::error::Error>> {
    for case in ["chain", "grant", "no_git", "budget", "empty_override"] {
        let project = TempDir::new()?;
        let cwd = project.path().join("component");
        std::fs::create_dir(&cwd)?;
        if case != "no_git" {
            std::fs::write(project.path().join(".git"), "gitdir: fixture")?;
        }
        std::fs::write(project.path().join("AGENTS.md"), "ROOT_RULE")?;
        std::fs::write(cwd.join("AGENTS.md"), "CHILD_RULE")?;
        if case == "empty_override" {
            std::fs::write(cwd.join("AGENTS.override.md"), " \n")?;
        }
        let read_root = if case == "grant" {
            &cwd
        } else {
            project.path()
        };
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn()])?,
            &[read_root.into(), cwd.clone()],
            store.clone(),
        )?
        .with_resources(HarnessConfig {
            instructions: InstructionConfig {
                project_doc_max_bytes: if case == "budget" { 4 } else { 32 * 1024 },
                ..Default::default()
            },
            ..Default::default()
        })?;
        let mut request = request(&project);
        request.workspace = cwd;
        let accepted = service.submit_fixture(request).await?;
        wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
        let requests = prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
        let body = startup(&requests[0]).ok_or("startup context")?;
        match case {
            "grant" | "no_git" => {
                assert!(body.contains("CHILD_RULE"));
                assert!(!body.contains("ROOT_RULE"));
            }
            "budget" => {
                assert!(body.contains("ROOT"));
                assert!(!body.contains("ROOT_RULE"));
                assert!(!body.contains("CHILD_RULE"));
            }
            "empty_override" => {
                assert!(body.contains("ROOT_RULE"));
                assert!(!body.contains("CHILD_RULE"));
            }
            _ => {
                assert!(body.contains("ROOT_RULE"));
                assert!(body.contains("CHILD_RULE"));
            }
        }
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn agents_md_counts_toward_the_model_context_bound() -> Result<(), Box<dyn std::error::Error>>
{
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("AGENTS.md"), "x".repeat(4096))?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        store.clone(),
    )?;
    let mut request = request(&workspace);
    request.config.max_context_bytes = 4096;
    let accepted = service.submit_fixture(request).await?;
    let done = wait_for(&service, &accepted.turn_id, TurnStatus::Failed).await?;
    assert_eq!(done.status, TurnStatus::Failed);
    assert!(
        done.detail
            .as_deref()
            .is_some_and(|detail| detail.contains("model context"))
    );
    assert!(
        prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id)
            .await?
            .is_empty()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn live_thread_reuses_startup_snapshot_and_new_thread_rediscovers()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("AGENTS.md"), "ORIGINAL_PROJECT_RULES")?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_workspace_grants(
        app(vec![
            turn(vec![tool_call(
                "replace",
                "write",
                json!({"path":"AGENTS.md", "content":"UPDATED_PROJECT_RULES"}),
            )]),
            final_turn(),
            final_turn(),
            final_turn(),
        ])?,
        &[WorkspaceGrant {
            workspace: workspace.path().into(),
            permission_profiles: vec![PermissionProfile::AllowEffects],
        }],
        store.clone(),
    )?;
    let mut request = thread_request(&workspace, "create");
    request.permission_profile = PermissionProfile::AllowEffects;
    let created = service
        .create_thread(&service.inner.instance_id, request)
        .await?;
    let target = target(&created);
    let caller = CallerContext::local();
    for key in ["first", "second"] {
        if key == "second" {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while service
                    .lock_state()
                    .running_turns
                    .values()
                    .any(|thread_id| thread_id == &created.thread_id)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            service.unload_thread(&target, &caller).await?;
            let loaded = service.load_thread(&target, &caller).await?;
            assert!(loaded.recovery.is_none());
        }
        let accepted = service
            .start_turn(&target, &caller, input("inspect", key))
            .await?;
        assert_eq!(
            wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
                .await?
                .status,
            TurnStatus::Completed
        );
        for prompt in prompts(store.as_ref(), &created.thread_id, &accepted.turn_id).await? {
            assert!(
                startup(&prompt).is_some_and(|text| text.contains("ORIGINAL_PROJECT_RULES")
                    && !text.contains("UPDATED_PROJECT_RULES"))
            );
        }
    }
    let saved = store.load(&created.thread_id).await?.ok_or("journal")?;
    assert_eq!(
        saved
            .records
            .iter()
            .filter(|record| matches!(
                turn_fact(record),
                ExecutionRecord::InstructionContext { .. }
            ))
            .count(),
        1
    );
    let mut request = thread_request(&workspace, "new");
    request.permission_profile = PermissionProfile::AllowEffects;
    let fresh = service
        .create_thread(&service.inner.instance_id, request)
        .await?;
    let accepted = service
        .start_turn(
            &super::support::target(&fresh),
            &caller,
            input("inspect", "third"),
        )
        .await?;
    assert_eq!(
        wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let requests = prompts(store.as_ref(), &fresh.thread_id, &accepted.turn_id).await?;
    assert!(startup(&requests[0]).is_some_and(|text| text.contains("UPDATED_PROJECT_RULES")));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn empty_startup_snapshot_is_cached_until_a_new_thread()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn(), final_turn(), final_turn()])?,
        &[workspace.path().into()],
        store.clone(),
    )?;
    let created = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let caller = CallerContext::local();
    for key in ["first", "second"] {
        let accepted = service
            .start_turn(&target(&created), &caller, input("inspect", key))
            .await?;
        wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
        let requests = prompts(store.as_ref(), &created.thread_id, &accepted.turn_id).await?;
        assert!(startup(&requests[0]).is_none());
        std::fs::write(workspace.path().join("AGENTS.md"), "LATER_PROJECT_RULE")?;
    }
    let fresh = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "new"),
        )
        .await?;
    let accepted = service
        .start_turn(&target(&fresh), &caller, input("inspect", "third"))
        .await?;
    wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    let requests = prompts(store.as_ref(), &fresh.thread_id, &accepted.turn_id).await?;
    assert!(startup(&requests[0]).is_some_and(|body| body.contains("LATER_PROJECT_RULE")));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn deeper_instructions_enter_context_through_native_read()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::create_dir(workspace.path().join("nested"))?;
    std::fs::write(workspace.path().join("AGENTS.md"), "ROOT_RULE")?;
    std::fs::write(
        workspace.path().join("nested/AGENTS.override.md"),
        "NESTED_OVERRIDE_RULE",
    )?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "nested_rules",
                "read",
                json!({"path":"nested/AGENTS.override.md"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
        store.clone(),
    )?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    let requests = prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
    assert_eq!(requests.len(), 2);
    assert!(startup(&requests[0]).is_some_and(|body| body.contains("ROOT_RULE")));
    assert!(!serde_json::to_string(&requests[0].messages)?.contains("NESTED_OVERRIDE_RULE"));
    assert!(
        requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(
                |content| matches!(content, Content::ToolResult { call_id, output, .. }
            if call_id == "nested_rules" && serde_json::to_string(output).is_ok_and(
                |text| text.contains("NESTED_OVERRIDE_RULE")))
            )
    );
    let journal = store.load(&accepted.thread_id).await?.ok_or("journal")?;
    assert!(
        journal
            .records
            .iter()
            .any(|record| matches!(turn_fact(record),
            ExecutionRecord::ToolResult { message, .. }
                if message.content.iter().any(|content| matches!(content,
                    Content::ToolResult { call_id, .. } if call_id == "nested_rules"
                ))
            ))
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn corrupt_or_unsettled_instruction_context_blocks_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("AGENTS.md"), "PROJECT_RULE")?;
    let source_store = Arc::new(MemoryExecutionStore::default());
    let source = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        source_store.clone(),
    )?;
    let accepted = source.submit_fixture(request(&workspace)).await?;
    wait_for(&source, &accepted.turn_id, TurnStatus::Completed).await?;
    source.shutdown().await;
    let saved = source_store
        .load(&accepted.thread_id)
        .await?
        .ok_or("journal")?;
    for case in ["source", "message", "version", "unsettled"] {
        let mut records = saved.records.clone();
        let mut changed = false;
        for record in &mut records {
            if let ExecutionRecord::TurnRecord { fact, .. } = record
                && let ExecutionRecord::InstructionContext {
                    context_version,
                    snapshot,
                    message,
                    ..
                } = fact.as_mut()
            {
                match case {
                    "source" => snapshot.cwd = workspace.path().join("different"),
                    "message" => {
                        *message = Some(bitrouter_sdk::language_model::Message::text(
                            Role::User,
                            "forged instructions",
                        ))
                    }
                    "version" => *context_version += 1,
                    _ => {}
                }
                changed = true;
            }
        }
        assert!(changed);
        if case == "unsettled" {
            let instruction = records
                .iter()
                .position(|record| {
                    matches!(
                        turn_fact(record),
                        ExecutionRecord::InstructionContext { .. }
                    )
                })
                .ok_or("instruction fact")?;
            let instruction = records.remove(instruction);
            let model = records
                .iter()
                .position(|record| {
                    matches!(turn_fact(record), ExecutionRecord::ModelRequest { .. })
                })
                .ok_or("model request")?;
            records.insert(model + 1, instruction);
        }
        let store = Arc::new(MemoryExecutionStore::default());
        store.commit(&accepted.thread_id, 0, &records).await?;
        let destination =
            ThreadService::with_store(app(vec![])?, &[workspace.path().into()], store)?;
        let target = crate::thread::ThreadTarget {
            thread_id: accepted.thread_id.clone(),
            server_instance_id: destination.inner.instance_id.clone(),
        };
        let view = destination
            .load_thread(&target, &CallerContext::local())
            .await?;
        let recovery = view.recovery.as_ref().ok_or("recovery report")?;
        assert!(!recovery.context_valid);
        assert!(recovery.blockers.iter().any(|blocker| matches!(blocker,
            crate::thread::RecoveryBlocker::InvalidRecord { detail }
                if detail.contains("startup instruction")
        )));
        assert!(
            destination
                .recover_thread(
                    &target,
                    &CallerContext::local(),
                    crate::thread::ThreadRecoveryRequest {
                        source_server_instance_id: recovery.source_server_instance_id.clone(),
                        source_cursor: recovery.source_cursor,
                        idempotency_key: "invalid-recovery".into(),
                    },
                )
                .await
                .is_err()
        );
        destination.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn local_registration_reads_ancestors_without_expanding_tool_permissions()
-> Result<(), Box<dyn std::error::Error>> {
    let project = TempDir::new()?;
    let cwd = project.path().join("component");
    std::fs::create_dir(&cwd)?;
    std::fs::write(project.path().join(".git"), "gitdir: fixture")?;
    std::fs::write(project.path().join("AGENTS.md"), "PARENT_RULE")?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "escape",
                "read",
                json!({"path":"../AGENTS.md"}),
            )]),
            final_turn(),
        ])?,
        &[],
        store.clone(),
    )?;
    service.register_local_workspace(&cwd)?;
    let mut request = request(&project);
    request.workspace = cwd;
    let accepted = service.submit_fixture(request).await?;
    assert_eq!(
        wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let requests = prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
    assert!(startup(&requests[0]).is_some_and(|text| text.contains("PARENT_RULE")));
    assert!(
        requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content,
                Content::ToolResult { call_id, output: ToolResultOutput::ErrorJson { value }, .. }
                    if call_id == "escape" && value.get("error").is_some_and(|error|
                        error.as_str().is_some_and(|text| !text.is_empty()))
            ))
    );
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn instruction_symlinks_stay_within_the_host_read_root()
-> Result<(), Box<dyn std::error::Error>> {
    for outside in [false, true] {
        let workspace = TempDir::new()?;
        let other = TempDir::new()?;
        let destination = if outside {
            other.path()
        } else {
            workspace.path()
        }
        .join("rules.md");
        std::fs::write(&destination, "LINKED_PROJECT_RULES")?;
        std::os::unix::fs::symlink(&destination, workspace.path().join("AGENTS.md"))?;
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn()])?,
            &[workspace.path().into()],
            store.clone(),
        )?;
        let accepted = service.submit_fixture(request(&workspace)).await?;
        let expected = if outside {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        assert_eq!(
            wait_for(&service, &accepted.turn_id, expected)
                .await?
                .status,
            expected
        );
        let requests = prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
        if outside {
            assert!(requests.is_empty());
        } else {
            assert!(
                startup(&requests[0]).is_some_and(|text| text.contains("LINKED_PROJECT_RULES"))
            );
        }
        service.shutdown().await;
    }
    Ok(())
}
