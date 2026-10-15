//! Structural boundaries formerly enforced by separate renderer packages.

use std::path::{Path, PathBuf};

fn sources(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(sources(&path)?);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[test]
fn terminal_views_use_only_commands_and_presentation() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");
    let files = sources(&root)?;
    assert!(!files.is_empty(), "renderer inventory must not be empty");
    for path in files {
        let source = std::fs::read_to_string(&path)?;
        for (offset, _) in source.match_indices("crate::") {
            let reference = &source[offset + "crate::".len()..];
            assert!(
                [
                    "tui::",
                    "commands::",
                    "presentation::",
                    "harness::permission::"
                ]
                .iter()
                .any(|prefix| reference.starts_with(prefix)),
                "{} reaches service state outside the renderer boundary at byte {offset}",
                path.display(),
            );
        }
        for forbidden in [
            "use crate as ",
            "use crate::{",
            "reqwest::",
            "sea_orm::",
            "bitrouter_sdk::",
            "std::process::Command",
            "tokio::process::Command",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} introduces service execution through {forbidden}",
                path.display(),
            );
        }
    }
    Ok(())
}

#[test]
fn shared_presentation_and_policy_are_terminal_independent()
-> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = sources(&root.join("presentation"))?;
    files.extend([
        root.join("commands/chat.rs"),
        root.join("commands/agents.rs"),
        root.join("harness/permission.rs"),
        root.join("output/plain.rs"),
    ]);
    for path in files {
        let source = std::fs::read_to_string(&path)?;
        for forbidden in [
            "ratatui::",
            "ratatui_markdown::",
            "crossterm::",
            "crate::tui::",
            "sea_orm::",
            "reqwest::",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} couples shared presentation or policy to {forbidden}",
                path.display(),
            );
        }
    }
    Ok(())
}
