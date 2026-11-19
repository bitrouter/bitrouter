//! Object-level assertions on disposable workspace files.
use super::WorkspaceTools;
#[cfg(unix)]
use super::files::{Node, contents};
use crate::workspace::children::Parent;
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, ensure};
use bitrouter_ai::harness::{agent::ToolMode, runtime::EffectStatus};
use std::ffi::OsStr;
use std::fs;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
#[test]
fn pinned_descendant_keeps_original_file_and_publication_directory() -> Result<()> {
    let tmp = tempdir()?;
    let root = tmp.path();
    fs::create_dir_all(root.join("a/b"))?;
    fs::write(root.join("a/b/note"), b"original")?;
    let tools = WorkspaceTools::new(root, ToolMode::Coding)?;
    let mut effect = EffectStatus::NotExecuted;
    let (_, Node::File(mut reader)) = tools.lookup(Some("a/b/note"))? else {
        anyhow::bail!("expected note")
    };
    let mut writer = tools.destination("a/b/note", false, &mut effect)?;
    let original = writer.original()?.context("missing original")?;
    fs::rename(root.join("a"), root.join("moved"))?;
    fs::create_dir_all(root.join("a/b"))?;
    fs::write(root.join("a/b/note"), b"replacement")?;
    assert_eq!(contents(&mut reader)?, b"original");
    writer.publish(b"updated", Some(&original), &mut effect)?;
    assert_eq!(fs::read(root.join("moved/b/note"))?, b"updated");
    assert_eq!(fs::read(root.join("a/b/note"))?, b"replacement");
    Ok(())
}

#[test]
fn pending_new_file_never_overwrites_a_new_arrival() -> Result<()> {
    let tmp = tempdir()?;
    let tools = WorkspaceTools::new(tmp.path(), ToolMode::Coding)?;
    let mut effect = EffectStatus::NotExecuted;
    let dest = tools.destination("note", true, &mut effect)?;
    fs::write(tmp.path().join("note"), b"first creator")?;
    ensure!(dest.publish(b"other", None, &mut effect).is_err());
    assert_eq!(effect, EffectStatus::NotExecuted);
    assert_eq!(fs::read(tmp.path().join("note"))?, b"first creator");
    Ok(())
}

#[test]
fn nested_directory_inventory_keeps_its_open_parent() -> Result<()> {
    let tmp = tempdir()?;
    let root = tmp.path();
    fs::create_dir_all(root.join("project/src"))?;
    fs::write(root.join("project/src/selected"), b"selected")?;
    let tools = WorkspaceTools::new(root, ToolMode::Coding)?;
    let dir = Parent::root(&tools.directory)?
        .child(OsStr::new("project"))?
        .child(OsStr::new("src"))?;

    let moved = fs::rename(root.join("project"), root.join("moved"));
    #[cfg(unix)]
    {
        moved?;
        fs::create_dir_all(root.join("project/src"))?;
        fs::write(root.join("project/src/replacement"), b"replacement")?;
    }
    #[cfg(windows)]
    ensure!(moved.is_err(), "directory ancestry was not pinned");

    for _ in 0..2 {
        let entries = dir.entries()?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "selected");
    }
    Ok(())
}

#[test]
fn writing_existing_file_preserves_its_object_and_truncates_old_bytes() -> Result<()> {
    use std::io::Read;

    let tmp = tempdir()?;
    let path = tmp.path().join("note");
    fs::write(&path, b"original content")?;
    let tools = WorkspaceTools::new(tmp.path(), ToolMode::Coding)?;
    let mut observer = fs::File::open(&path)?;
    let mut effect = EffectStatus::NotExecuted;
    let dest = tools.destination("note", false, &mut effect)?;
    dest.publish(b"new", Some(b"original content"), &mut effect)?;
    let mut observed = Vec::new();
    observer.read_to_end(&mut observed)?;
    assert_eq!(observed, b"new");
    assert_eq!(fs::read(path)?, b"new");
    Ok(())
}

#[cfg(unix)]
#[test]
fn explicit_project_aliases_keep_their_read_and_parent_write_behavior() -> Result<()> {
    use std::os::unix::fs::symlink;

    let tmp = tempdir()?;
    let root = tmp.path();
    fs::create_dir(root.join("src"))?;
    fs::write(root.join("src/note"), b"original")?;
    symlink("src/note", root.join("note-alias"))?;
    symlink("src", root.join("src-alias"))?;
    let tools = WorkspaceTools::new(root, ToolMode::Coding)?;

    let (_, Node::File(mut reader)) = tools.lookup(Some("note-alias"))? else {
        anyhow::bail!("expected an explicit file alias");
    };
    assert_eq!(contents(&mut reader)?, b"original");
    let mut effect = EffectStatus::NotExecuted;
    ensure!(tools.destination("note-alias", false, &mut effect).is_err());
    tools
        .destination("src-alias/new", true, &mut effect)?
        .publish(b"added", None, &mut effect)?;
    assert_eq!(fs::read(root.join("src/new"))?, b"added");
    Ok(())
}

#[test]
fn existing_replacement_with_identical_content_keeps_its_identity() -> Result<()> {
    let tmp = tempdir()?;
    let original = tmp.path().join("note");
    fs::write(&original, b"identical")?;
    let tools = WorkspaceTools::new(tmp.path(), ToolMode::Coding)?;
    let mut effect = EffectStatus::NotExecuted;
    let dest = tools.destination("note", false, &mut effect)?;
    let moved = fs::rename(&original, tmp.path().join("first"));
    #[cfg(windows)]
    {
        ensure!(moved.is_err(), "pinned file allowed rename");
        dest.publish(b"legitimate", Some(b"identical"), &mut effect)?;
        assert_eq!(fs::read(original)?, b"legitimate");
    }
    #[cfg(unix)]
    {
        moved?;
        fs::write(&original, b"identical")?;
        ensure!(
            dest.publish(b"changed", Some(b"identical"), &mut effect)
                .is_err()
        );
        assert_eq!(effect, EffectStatus::NotExecuted);
        assert_eq!(fs::read(original)?, b"identical");
        assert_eq!(fs::read(tmp.path().join("first"))?, b"identical");
    }
    Ok(())
}

#[tokio::test]
async fn multiple_links_cannot_supply_content_or_write_authority() -> Result<()> {
    let original = tempdir()?;
    let grant = tempdir()?;
    let file = original.path().join("original");
    fs::write(&file, b"shared fixture bytes")?;
    fs::hard_link(&file, grant.path().join("alias"))?;
    let tools = WorkspaceTools::new(grant.path(), ToolMode::Coding)?;
    for (tool, args) in [
        ("read", serde_json::json!({"path":"alias"})),
        (
            "write",
            serde_json::json!({"path":"alias","content":"update"}),
        ),
        (
            "edit",
            serde_json::json!({"path":"alias","edits":[{"oldText":"fixture","newText":"changed"}]}),
        ),
    ] {
        let (result, effect) = tools
            .execute_with_effect(
                tool,
                &args.to_string(),
                &CancellationToken::new(),
                "fixture",
                None,
            )
            .await;
        ensure!(
            result.is_error() && effect == EffectStatus::NotExecuted,
            "{tool}: {result:?}"
        );
        assert_eq!(fs::read(&file)?, b"shared fixture bytes");
        assert_eq!(
            fs::read(grant.path().join("alias"))?,
            b"shared fixture bytes"
        );
    }
    fs::write(grant.path().join("own"), b"fixture line")?;
    let result = tools
        .execute(
            "grep",
            r#"{"pattern":"fixture"}"#,
            &CancellationToken::new(),
            "fixture",
            None,
        )
        .await;
    assert!(
        matches!(result, bitrouter_ai::types::ToolResultOutput::Text { value }
        if value == "own:1: fixture line\n")
    );
    Ok(())
}
