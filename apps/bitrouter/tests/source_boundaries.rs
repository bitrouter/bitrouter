//! Structural boundaries formerly enforced by separate renderer packages.

use std::path::Path;

#[test]
fn shared_commands_and_policy_are_terminal_independent() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for relative in [
        "commands/chat.rs",
        "commands/agent_options.rs",
        "skills/protocol.rs",
        "storage/skills/state.rs",
    ] {
        let path = root.join(relative);
        let source = std::fs::read_to_string(&path)?;
        for forbidden in [
            "ratatui::",
            "crossterm::",
            "crate::tui::",
            "sea_orm::",
            "reqwest::",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} imports {forbidden}",
                path.display()
            );
        }
    }
    Ok(())
}

#[test]
fn local_task_client_does_not_import_execution_or_storage() -> Result<(), Box<dyn std::error::Error>>
{
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/client/agent_local.rs");
    let source = std::fs::read_to_string(&path)?;
    for forbidden in [
        "bitrouter::harness::",
        "crate::db::",
        "crate::agent_store::",
        "sea_orm::",
        "agent_client_protocol::",
    ] {
        assert!(
            !source.contains(forbidden),
            "{} couples task transport to {forbidden}",
            path.display(),
        );
    }
    Ok(())
}
