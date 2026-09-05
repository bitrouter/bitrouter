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
