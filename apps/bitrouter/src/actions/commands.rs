//! What commands a session offers, and who answers each one.
//!
//! The one action whose subject is a live session and whose answer still has
//! two surfaces: the interactive `/commands`, and a headless leaf that opens a
//! session of its own to ask. They share this type so the two cannot describe
//! the same session differently.
//!
//! The rows are **not merged into one namespace**. Each carries the source that
//! answers it, and a name answered by more than one source appears once per
//! source with the losing rows marked [`CommandRow::shadowed`]. Listing the
//! shadowed row rather than dropping it is what makes the resolver's precedence
//! visible: an agent that advertises `status` is not silently ignored, it is
//! shown as reachable-by-another-name.

/// Who answers a command.
///
/// The order of the variants is the precedence order — BitRouter's own name
/// wins, then a user's configured one, then the agent's.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    /// A row in the actions table, answered locally through its port.
    Bitrouter,
    /// A prompt-expansion command from `bitrouter.yaml`. It expands to a
    /// prompt; it runs nothing.
    Config,
    /// Advertised by the agent over ACP, and answered by the agent.
    Agent,
}

/// One command a session offers.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct CommandRow {
    /// Without the leading slash, and never rewritten — no sigil, no prefix.
    /// A reader who sees `compact` here can type `/compact`.
    pub name: String,
    /// One line saying what it does.
    pub description: String,
    /// ACP's `input.hint`, for a command that takes an argument. Agent rows
    /// only; `None` when the agent gave none.
    pub hint: Option<String>,
    /// Who answers it.
    pub source: CommandSource,
    /// A higher-precedence source offers this same name, so typing it reaches
    /// that one instead. Listed rather than dropped.
    pub shadowed: bool,
    /// Why this command cannot run in this session, when it cannot. Listed
    /// with its reason rather than hidden — a control that has gone missing
    /// teaches nothing.
    pub unavailable: Option<String>,
}

/// Every command a session offers, and whether the agent had answered yet.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct CommandsReport {
    /// Whether an `available_commands_update` arrived at all.
    ///
    /// The distinction this exists for: `false` with no agent rows means the
    /// agent said nothing before the deadline; `true` with no agent rows means
    /// the agent said *none*. The protocol makes both possible and they mean
    /// different things — the first may be worth waiting longer for, the second
    /// never is.
    pub received: bool,
    /// The commands, in precedence order and then in source order.
    pub commands: Vec<CommandRow>,
}

use agent_client_protocol::schema::v1::{AvailableCommand, AvailableCommandInput};
use bitrouter_tui::machine::{Command, PromptCommand};

/// Everything this session offers, in precedence order.
///
/// `received` says whether an `available_commands_update` arrived at all — the
/// caller knows, and the report cannot infer it from an empty list.
///
/// Takes ACP's own [`AvailableCommand`] rather than the SDK's translated
/// `AgentCommand` because only the former carries `input`, and the hint is
/// worth showing for a command that takes an argument.
pub fn commands_report(
    bitrouter: &[Command],
    config: &[PromptCommand],
    agent: &[AvailableCommand],
    received: bool,
) -> CommandsReport {
    let mut commands: Vec<CommandRow> = bitrouter
        .iter()
        .map(|command| CommandRow {
            name: command.name.to_string(),
            description: command.summary.to_string(),
            hint: None,
            source: CommandSource::Bitrouter,
            // Nothing outranks a local name: a config command that claimed
            // one is refused when the config loads.
            shadowed: false,
            unavailable: command.unavailable.map(str::to_string),
        })
        .collect();
    // Between BitRouter's and the agent's, which is where its precedence
    // sits. A config row is never shadowed — a name BitRouter answers is
    // refused at load, so the only clash it can win is against the agent.
    commands.extend(config.iter().map(|command| CommandRow {
        name: command.name.clone(),
        description: command.description.clone(),
        hint: None,
        source: CommandSource::Config,
        shadowed: false,
        unavailable: None,
    }));
    commands.extend(agent.iter().map(|command| {
        CommandRow {
            name: command.name.clone(),
            description: command.description.clone(),
            hint: hint_of(command),
            source: CommandSource::Agent,
            // Typing this name reaches BitRouter's, so say so rather than listing
            // two rows that look equally reachable.
            shadowed: bitrouter
                .iter()
                .any(|ours| ours.name == command.name.as_str())
                || config.iter().any(|theirs| theirs.name == command.name),
            unavailable: None,
        }
    }));
    CommandsReport { received, commands }
}

/// The hint an agent attached to a command that takes an argument.
///
/// `AvailableCommandInput` is `#[non_exhaustive]`: a variant this build does
/// not know about yields no hint rather than failing to compile against a
/// newer schema.
fn hint_of(command: &AvailableCommand) -> Option<String> {
    match command.input.as_ref()? {
        AvailableCommandInput::Unstructured(input) => Some(input.hint.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours(name: &'static str, unavailable: Option<&'static str>) -> Command {
        Command {
            name,
            action: name,
            summary: "does a thing",
            unavailable,
        }
    }

    fn theirs(name: &str) -> AvailableCommand {
        AvailableCommand::new(name, "the agent's own")
    }

    /// BitRouter's rows come first, and each row says who answers it.
    #[test]
    fn rows_are_grouped_by_source_in_precedence_order() {
        let report = commands_report(&[ours("route", None)], &[], &[theirs("compact")], true);
        let sources: Vec<_> = report.commands.iter().map(|row| row.source).collect();
        assert_eq!(
            sources,
            [CommandSource::Bitrouter, CommandSource::Agent],
            "ours is listed before theirs"
        );
        assert!(report.commands.iter().all(|row| !row.name.starts_with('/')));
    }

    /// A name both answer is listed twice, and the losing row says so. The
    /// alternative — dropping it — hides the precedence rule from the reader.
    #[test]
    fn a_clashing_agent_command_is_listed_and_marked_shadowed() {
        let report = commands_report(
            &[ours("status", None)],
            &[],
            &[theirs("status"), theirs("compact")],
            true,
        );
        let shadowed: Vec<_> = report
            .commands
            .iter()
            .filter(|row| row.shadowed)
            .map(|row| (row.name.as_str(), row.source))
            .collect();
        assert_eq!(shadowed, [("status", CommandSource::Agent)]);
        assert_eq!(report.commands.len(), 3, "nothing is dropped");
    }

    /// A command that cannot run keeps its reason all the way into the report.
    #[test]
    fn an_unavailable_command_carries_its_reason() {
        let report = commands_report(&[ours("route", Some("no route control"))], &[], &[], true);
        assert_eq!(
            report.commands[0].unavailable.as_deref(),
            Some("no route control")
        );
    }

    /// "said none" and "has not said yet" are different answers, and the
    /// report distinguishes them where an empty list alone could not.
    #[test]
    fn silence_and_an_empty_list_are_different_answers() {
        let said_none = commands_report(&[], &[], &[], true);
        let silent = commands_report(&[], &[], &[], false);
        assert!(said_none.received && said_none.commands.is_empty());
        assert!(!silent.received && silent.commands.is_empty());

        let rendered = |report: &CommandsReport| {
            String::from_utf8_lossy(
                &crate::output::Output::new(crate::output::Format::Human).render_to_vec(report),
            )
            .to_string()
        };
        assert!(rendered(&said_none).contains("advertises no commands"));
        assert!(rendered(&silent).contains("had not sent"));
    }

    /// The rendered view is tab-free: it is drawn into the session's notice,
    /// where a tab desynchronises the differential writer's column arithmetic.
    #[test]
    fn the_rendered_view_carries_no_tabs() {
        let report = commands_report(
            &[ours("route", Some("no route control"))],
            &[],
            &[theirs("route"), theirs("compact")],
            true,
        );
        let text = String::from_utf8_lossy(
            &crate::output::Output::new(crate::output::Format::Human).render_to_vec(&report),
        )
        .to_string();
        assert!(!text.contains('\t'), "{text:?}");
        assert!(text.contains("/compact"), "{text:?}");
        assert!(text.contains("shadowed"), "{text:?}");
        assert!(text.contains("no route control"), "{text:?}");
    }
}
