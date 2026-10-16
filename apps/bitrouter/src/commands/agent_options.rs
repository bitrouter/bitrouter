//! Shared agent CLI options and negotiated capability data.

/// Per-invocation routing decision for a spawned sub-agent. Routing is on by
/// default; `direct` opts out. See `docs/SPAWN_SPEC.md` §5.
#[derive(
    clap::Args, Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub struct RoutingOptions {
    /// Do NOT route this session's LLM traffic through the daemon — let the
    /// harness use its own provider auth. Routing is attempted by default
    /// when the harness supports headless redirection.
    #[arg(long)]
    pub direct: bool,
    /// Override the gateway base URL (else derived from `server.listen`).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Pin the harness's model (via its model env var / `-c model=`).
    #[arg(long)]
    pub model: Option<String>,
    /// Never auto-start a local daemon when none is running — fail fast.
    #[arg(long)]
    pub no_start: bool,
}

/// What a headless `prompt` prints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum PromptFormat {
    /// One self-describing JSON object per line.
    #[default]
    #[value(alias = "json")]
    Ndjson,
    /// The transcript as the piped `bro chat <agent>` compatibility path prints it.
    Text,
    /// The assistant's text and nothing else.
    Quiet,
}

/// The flags a headless caller states instead of sitting at the terminal:
/// how permission requests are answered, and what is printed.
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadlessOptions {
    /// Approve every permission request the harness makes.
    #[arg(long, conflicts_with_all = ["approve_reads", "deny_all"])]
    pub approve_all: bool,
    /// Approve tool calls the harness labels `read` or `search` (the ACP tool
    /// kind); deny everything else, including unlabelled calls.
    #[arg(long, conflicts_with_all = ["approve_all", "deny_all"])]
    pub approve_reads: bool,
    /// Deny every permission request. This is the default.
    #[arg(long, conflicts_with_all = ["approve_all", "approve_reads"])]
    pub deny_all: bool,
    /// Per-tool policy, inline JSON or `@path`:
    /// `{"autoApprove":[…],"autoDeny":[…],"defaultAction":"approve"|"deny"}`.
    /// Entries match the ACP tool kind (`read`, `edit`, `execute`, …), the
    /// tool-call title, or its first word. `autoDeny` wins over `autoApprove`;
    /// an unmatched request uses `defaultAction`, else the mode flag.
    #[arg(long, value_name = "JSON|@PATH")]
    pub permission_policy: Option<String>,
    /// Output: `ndjson` (the default), `text` (the transcript as
    /// `code <agent>` prints it to a pipe), or `quiet` (assistant text only).
    #[arg(long, value_enum, default_value_t)]
    pub format: PromptFormat,
}

/// Which harness-native session a one-shot driver opens.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionSelection {
    /// Create a new harness-native session.
    #[default]
    New,
    /// Load a session and replay its history.
    Load(String),
    /// Resume a session without replaying its history.
    Resume(String),
}

/// Negotiated facts shared by every BitRouter-owned ACP client surface.
///
/// Drivers render or reject controls from this value; they do not infer
/// support from an agent name or optimistically send draft methods.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapabilitySnapshot {
    pub load: bool,
    pub resume: bool,
    pub list: bool,
    pub close: bool,
    pub delete: bool,
    pub fork: bool,
    pub mcp_stdio: bool,
    pub mcp_http: bool,
    pub mcp_sse: bool,
    pub authentication_methods: Vec<String>,
    pub terminal_auth: bool,
    pub route_list: bool,
    pub route_set: bool,
    pub route_reset: bool,
}

impl CapabilitySnapshot {
    /// Compact lifecycle explanation used by the Code TUI.
    pub fn lifecycle_summary(&self) -> String {
        let mut supported = vec!["new"];
        for (name, enabled) in [
            ("list", self.list),
            ("load", self.load),
            ("resume", self.resume),
            ("close", self.close),
            ("fork*", self.fork),
            ("delete*", self.delete),
        ] {
            if enabled {
                supported.push(name);
            }
        }
        supported.join(", ")
    }
}
