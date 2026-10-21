//! Session command descriptors and pure prompt-template resolution.
//! Applications validate and construct configured templates; resolution executes no action.

/// One of BitRouter's own slash commands, as the session client needs to know it.
///
/// Built by the app from the `ACTIONS` rows that carry a `session_command`; the
/// session client never sees the table, so it cannot offer a command the table does
/// not have. That is the whole of the coupling: this module reads no application configuration, so the set of commands is data handed in, exactly as
/// `routable` was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The words after the slash, space-separated: `"status"`, `"route reset"`.
    pub name: &'static str,
    /// The `ACTIONS` row id, matched by the session client only for the ids in
    /// [`CLIENT_OWNED`].
    pub action: &'static str,
    /// One line for `/commands`.
    pub summary: &'static str,
    /// `Some(reason)` when the row's requirement is unmet in this session. The
    /// command is still listed — with the reason — and typing it answers with
    /// the reason instead of running. Absent, never dead.
    pub unavailable: Option<&'static str>,
}

/// A user-authored command that expands to a prompt.
///
/// Plain data: the session client substitutes and sends. It never runs anything, which
/// is why this registry can be open — the config is the user's, unreviewed —
/// while the commands that reach BitRouter's own ports stay a closed, guarded
/// table.
///
/// Held in a **separate field** from [`Command`], never merged into one map.
/// The two are checked against each other once, when the config loads, so no
/// runtime precedence rule between them is ever needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCommand {
    /// The word after the slash.
    pub name: String,
    /// One line for `/commands`.
    pub description: String,
    /// `$ARGUMENTS` is replaced by everything typed after the name.
    pub template: String,
}

/// Commands handled by the session client rather than an application action port.
/// Their presentation remains the caller's responsibility.
///
/// A guard asserts every entry is a row id that carries a `session_command`.
pub const CLIENT_OWNED: &[&str] = &["commands", "route_set", "route_reset"];

/// Names that mean another command. A guard asserts no alias is itself a
/// `session_command`, so an alias can never shadow a real name.
pub const ALIASES: &[(&str, &str)] = &[("help", "commands")];

/// What a submitted line turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A BitRouter command the session client runs itself.
    Owned {
        /// The `ACTIONS` row id.
        action: &'static str,
        /// The rest of the line, whitespace-split — never one string.
        args: Vec<String>,
    },
    /// A BitRouter command the app answers through its action ports.
    Action {
        /// The `ACTIONS` row id, handed back to the app unchanged.
        action: &'static str,
        /// The rest of the line, whitespace-split — never one string.
        args: Vec<String>,
    },
    /// A listed BitRouter command this session cannot run, and why.
    Unavailable(&'static str),
    /// A prompt-expansion command, already expanded. Sent as a turn.
    Expand(String),
    /// Not ours — a prompt, including any command the agent advertises.
    Prompt(String),
}

/// Resolve one submitted line against the commands this session offers.
///
/// Precedence is fixed here and nowhere else: a two-word BitRouter name, then a
/// one-word one, then an alias, then the agent. Local wins, which is why an
/// agent that advertises `/status` is shadowed rather than obeyed — `/commands`
/// says so rather than the shadowing being silent.
///
/// A free function over a slice, not a method, so the piped loop and the
/// headless one-shot path can run the same resolution the terminal does.
pub fn resolve(commands: &[Command], prompt_commands: &[PromptCommand], line: &str) -> Resolution {
    let original = line;
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return Resolution::Prompt(original.to_string());
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    // Longest name first, so `/route reset` is not `/route` with an argument.
    for take in [2, 1] {
        let Some(head) = words.get(..take) else {
            continue;
        };
        let typed = head.join(" ");
        let name = ALIASES
            .iter()
            .find(|(alias, _)| *alias == typed)
            .map_or(typed.as_str(), |(_, target)| *target);
        if let Some(command) = commands.iter().find(|candidate| candidate.name == name) {
            if let Some(reason) = command.unavailable {
                return Resolution::Unavailable(reason);
            }
            let args = words[take..].iter().map(|word| word.to_string()).collect();
            // The session client knows three verbs by name; everything else is an
            // opaque id the app resolves against the same ports the CLI uses.
            return if CLIENT_OWNED.contains(&command.action) {
                Resolution::Owned {
                    action: command.action,
                    args,
                }
            } else {
                Resolution::Action {
                    action: command.action,
                    args,
                }
            };
        }
    }
    // Only after every BitRouter name has missed. A config command can never
    // reach here under a name BitRouter answers — that config is refused at
    // load — so this order is a statement of precedence, not a tiebreak.
    if let Some(expansion) = words
        .first()
        .and_then(|name| prompt_commands.iter().find(|command| command.name == *name))
    {
        return Resolution::Expand(
            expansion
                .template
                .replace("$ARGUMENTS", &words[1..].join(" ")),
        );
    }
    Resolution::Prompt(original.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn commands_for(routable: bool) -> Vec<Command> {
        let unavailable = (!routable).then_some("this session cannot be rerouted");
        vec![
            Command {
                name: "commands",
                action: "commands",
                summary: "list the commands this session offers",
                unavailable: None,
            },
            Command {
                name: "route",
                action: "route_set",
                summary: "choose the route for the rest of the session",
                unavailable,
            },
            Command {
                name: "route reset",
                action: "route_reset",
                summary: "drop the route lease",
                unavailable,
            },
        ]
    }

    #[test]
    fn prompt_fallback_preserves_composer_bytes() {
        for prompt in [
            "  中文 👩‍💻\n\ntrailing spaces  \n",
            "  /unknown  keep\n spacing  ",
        ] {
            assert_eq!(
                super::resolve(&[], &[], prompt),
                super::Resolution::Prompt(prompt.to_string())
            );
        }
    }

    /// Precedence, stated once and asserted here: the longest BitRouter name
    /// wins, an alias reaches its target, an unmet requirement answers with its
    /// reason, and anything else is the agent's business untouched.
    #[test]
    fn the_resolver_reads_the_longest_name_first_then_aliases_then_the_agent() {
        let offered = commands_for(true);
        assert_eq!(
            resolve(&offered, &[], "/route reset"),
            Resolution::Owned {
                action: "route_reset",
                args: Vec::new()
            },
            "`/route reset` is one two-word name, not `/route` with an argument"
        );
        assert_eq!(
            resolve(&offered, &[], "/route"),
            Resolution::Owned {
                action: "route_set",
                args: Vec::new()
            },
            "the one-word name is still reachable once the two-word one misses"
        );
        assert_eq!(
            resolve(&offered, &[], "/help"),
            Resolution::Owned {
                action: "commands",
                args: Vec::new()
            },
            "an alias reaches its target"
        );
        assert_eq!(
            resolve(&offered, &[], "/plan ship it"),
            Resolution::Prompt("/plan ship it".to_string()),
            "a command we do not offer is the agent's, and is passed through whole"
        );
        assert_eq!(
            resolve(&offered, &[], "hello"),
            Resolution::Prompt("hello".to_string()),
            "a bare line is a prompt"
        );
        // Arguments are split, never handed over as one string.
        assert_eq!(
            resolve(&offered, &[], "/route  us-east   fast "),
            Resolution::Owned {
                action: "route_set",
                args: vec!["us-east".to_string(), "fast".to_string()]
            }
        );
        // A listed-but-unrunnable command answers with its reason.
        assert!(
            matches!(resolve(&commands_for(false), &[], "/route"), Resolution::Unavailable(reason) if reason.contains("cannot be rerouted"))
        );
    }

    /// A config command expands and is sent as a turn; a BitRouter name still
    /// wins, even against a config command that claims it.
    #[test]
    fn a_prompt_command_expands_and_never_outranks_a_bitrouter_name() {
        let offered = commands_for(true);
        let configured = vec![
            PromptCommand {
                name: "review".to_string(),
                description: "review a diff".to_string(),
                template: "Review this: $ARGUMENTS".to_string(),
            },
            // A config that names a BitRouter command is refused at load, so
            // this can only arise from a bug; the resolver must still not
            // prefer it.
            PromptCommand {
                name: "route".to_string(),
                description: "should never win".to_string(),
                template: "nope".to_string(),
            },
        ];
        assert_eq!(
            resolve(&offered, &configured, "/review the diff"),
            Resolution::Expand("Review this: the diff".to_string())
        );
        // No arguments substitutes the empty string rather than leaving the
        // placeholder visible in what is sent.
        assert_eq!(
            resolve(&offered, &configured, "/review"),
            Resolution::Expand("Review this: ".to_string())
        );
        assert_eq!(
            resolve(&offered, &configured, "/route"),
            Resolution::Owned {
                action: "route_set",
                args: Vec::new()
            },
            "local wins: the closed table is consulted before the open one"
        );
        assert_eq!(
            resolve(&offered, &configured, "/unknown thing"),
            Resolution::Prompt("/unknown thing".to_string()),
            "neither registry claims it, so it is the agent's"
        );
    }
}
