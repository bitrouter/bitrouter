//! Session command discovery and configured prompt expansion for the CLI.

use crate::actions::ACTIONS;

/// One line of help per row, for `/commands`.
///
/// App-side rather than on the row because no other surface reads it: the MCP
/// tool has its own description and the CLI leaf has clap's. A guard fails a
/// row that carries a `session_command` and has no arm here, so this cannot fall
/// behind the table.
pub fn summary_for(action: &str) -> &'static str {
    match action {
        "status" => "whether the daemon is up, and what it has spent",
        "list_models" => "the models this config can route to, optionally by provider",
        "route" => "show where a model would be routed, without sending anything",
        "commands" => "list the commands this session offers",
        "route_set" => "choose the route for the rest of the session",
        "route_reset" => "drop the route lease, so the daemon's default applies",
        _ => "",
    }
}

/// The user's prompt-expansion commands, or an error naming the first one that
/// collides with a command BitRouter answers.
///
/// **This is what lets the guards over `ACTIONS` stay exhaustive.** They are
/// silent about this registry, which is correct only if the two sets cannot
/// overlap — so the overlap is refused here, once, when the config loads,
/// rather than resolved by a precedence rule at every keystroke.
///
/// It cannot live in the SDK: the check needs the app-owned `ACTIONS` table.
pub fn prompt_commands(
    config: &crate::config::ChatConfig,
) -> anyhow::Result<Vec<crate::commands::chat::PromptCommand>> {
    let mut seen: Vec<&str> = Vec::new();
    let mut commands = Vec::with_capacity(config.commands.len());
    for command in &config.commands {
        let name = command.name.trim();
        anyhow::ensure!(
            !name.is_empty(),
            "a `chat.commands` entry has an empty name"
        );
        anyhow::ensure!(
            !name.contains(char::is_whitespace),
            "`chat.commands` name `{name}` contains whitespace; a command is one word"
        );
        // The resolver strips one slash before matching, so a name stored with
        // its slash could never be reached — it would go to the agent as a
        // prompt and be listed as `//name`. Refused rather than left silent.
        anyhow::ensure!(
            !name.starts_with('/'),
            "`chat.commands` name `{name}` starts with a slash; write it as \
             `{}` — the slash is how it is typed, not part of the name",
            name.trim_start_matches('/')
        );
        if let Some(row) = ACTIONS.iter().find(|row| row.session_command == Some(name)) {
            anyhow::bail!(
                "`chat.commands` defines `/{name}`, which is BitRouter's own `{}` command. \
                 Rename it: a config command may not shadow one that reaches BitRouter's ports",
                row.id
            );
        }
        if let Some((alias, target)) = crate::commands::chat::ALIASES
            .iter()
            .find(|(alias, _)| *alias == name)
        {
            anyhow::bail!(
                "`chat.commands` defines `/{alias}`, which is BitRouter's alias for \
                 `/{target}`. Rename it"
            );
        }
        anyhow::ensure!(
            !seen.contains(&name),
            "`chat.commands` defines `/{name}` twice"
        );
        seen.push(name);
        commands.push(crate::commands::chat::PromptCommand {
            name: name.to_string(),
            description: command.description.clone(),
            template: command.prompt.clone(),
        });
    }
    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config command may not take a name BitRouter answers.
    ///
    /// This is the check that lets the guards over `ACTIONS` stay exhaustive:
    /// they are silent about the open registry, which is only correct because
    /// the two sets cannot overlap.
    #[test]
    fn a_config_command_may_not_shadow_a_bitrouter_one() {
        use crate::config::{ChatConfig, PromptCommandConfig};

        let entry = |name: &str| PromptCommandConfig {
            name: name.to_string(),
            description: String::new(),
            prompt: "x".to_string(),
        };
        let commands = |names: &[&str]| ChatConfig {
            commands: names.iter().map(|name| entry(name)).collect(),
            ..Default::default()
        };

        let ok = prompt_commands(&commands(&["review", "ship"])).expect("no clash");
        assert_eq!(ok.len(), 2);

        // Every row that carries a `session_command` is refused by that name.
        for taken in ["status", "models", "preview", "commands", "route"] {
            let error = prompt_commands(&commands(&[taken]))
                .expect_err(&format!("`{taken}` is BitRouter's"));
            assert!(format!("{error}").contains(taken), "{error}");
        }
        // And so is an alias.
        let error = prompt_commands(&commands(&["help"])).expect_err("`help` is an alias");
        assert!(format!("{error}").contains("alias"), "{error}");

        // A duplicate would make one of the two unreachable.
        let error = prompt_commands(&commands(&["dup", "dup"])).expect_err("duplicate");
        assert!(format!("{error}").contains("twice"), "{error}");

        // A name with a space could never be typed as one word.
        let error = prompt_commands(&commands(&["two words"])).expect_err("whitespace");
        assert!(format!("{error}").contains("one word"), "{error}");
    }
}
