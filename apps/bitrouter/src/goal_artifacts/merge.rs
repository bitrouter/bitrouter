//! Reconstruct checked dependency ancestry before merging cumulative artifacts.

use std::collections::BTreeMap;

use super::*;
use crate::goal_runtime::{git, git_with_input};

pub(crate) struct Input<'a> {
    pub id: &'a str,
    pub dependencies: &'a [String],
    pub artifact: &'a ArtifactRef,
}

// Only hash-verified artifact trees participate. Worker-authored commits and
// the mutable source branch never provide integration ancestry.
pub(crate) async fn integrate(
    root: &Path,
    inputs: &[Input<'_>],
    workspace: &Path,
    base: &str,
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let temporary = tempfile::tempdir()?;
    let scratch = temporary.path().join("integration");
    git(
        workspace,
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            scratch.as_os_str(),
            base.as_ref(),
        ],
    )
    .await?;
    let result = build(root, inputs, &scratch, base).await;
    let cleanup = git(
        workspace,
        &[
            "worktree".as_ref(),
            "remove".as_ref(),
            "--force".as_ref(),
            scratch.as_os_str(),
        ],
    )
    .await;
    if let Err(error) = &cleanup {
        tracing::warn!(%error, "could not remove private artifact integration worktree");
    }
    let tree = result?;
    cleanup?;
    // Two-tree checkout fails instead of overwriting unrelated local edits.
    // HEAD remains the frozen Goal base so later capture retains its identity.
    git(
        workspace,
        &[
            "read-tree".as_ref(),
            "-m".as_ref(),
            "-u".as_ref(),
            base.as_ref(),
            tree.as_ref(),
        ],
    )
    .await?;
    Ok(())
}

async fn build(root: &Path, inputs: &[Input<'_>], scratch: &Path, base: &str) -> Result<String> {
    let mut commits = BTreeMap::<&str, String>::new();
    let mut merged = base.to_owned();
    while commits.len() < inputs.len() {
        let before = commits.len();
        for input in inputs {
            if commits.contains_key(input.id)
                || input
                    .dependencies
                    .iter()
                    .any(|id| !commits.contains_key(id.as_str()))
            {
                continue;
            }
            // Every bundle is cumulative against base. Materialize it once on
            // that base, then attach the actual checked dependency parents.
            git(
                scratch,
                &["reset".as_ref(), "--hard".as_ref(), base.as_ref()],
            )
            .await?;
            apply(root, input.artifact, scratch, base).await?;
            let tree = object_id(git(scratch, &["write-tree".as_ref()]).await?)?;
            let parents: Vec<&str> = if input.dependencies.is_empty() {
                vec![base]
            } else {
                input
                    .dependencies
                    .iter()
                    .map(|id| {
                        commits
                            .get(id.as_str())
                            .map(String::as_str)
                            .context("missing checked ancestor")
                    })
                    .collect::<Result<_>>()?
            };
            let commit = commit(scratch, &tree, &parents).await?;
            let tree = object_id(
                git(
                    scratch,
                    &[
                        "merge-tree".as_ref(),
                        "--write-tree".as_ref(),
                        merged.as_ref(),
                        commit.as_ref(),
                    ],
                )
                .await
                .with_context(|| {
                    format!(
                        "checked artifact {} could not merge with its dependency history",
                        input.id
                    )
                })?,
            )?;
            merged = self::commit(scratch, &tree, &[&merged, &commit]).await?;
            commits.insert(input.id, commit);
        }
        anyhow::ensure!(
            commits.len() > before,
            "artifact dependencies are missing, duplicated or cyclic"
        );
    }
    Ok(merged)
}

fn object_id(output: std::process::Output) -> Result<String> {
    let text = String::from_utf8(output.stdout)?;
    let id = text
        .lines()
        .next()
        .context("Git returned no integration object")?;
    anyhow::ensure!(
        (id.len() == 40 || id.len() == 64) && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Git integration object"
    );
    Ok(id.into())
}

async fn commit(workspace: &Path, tree: &str, parents: &[&str]) -> Result<String> {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "-c".as_ref(),
        "user.name=BRO".as_ref(),
        "-c".as_ref(),
        "user.email=bro@localhost".as_ref(),
        "-c".as_ref(),
        "commit.gpgsign=false".as_ref(),
        "commit-tree".as_ref(),
        tree.as_ref(),
    ];
    for parent in parents {
        args.push("-p".as_ref());
        args.push((*parent).as_ref());
    }
    object_id(git_with_input(workspace, &args, Some(b"Checked Goal artifact ancestry\n")).await?)
}
