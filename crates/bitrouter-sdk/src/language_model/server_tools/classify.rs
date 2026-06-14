//! Pure classification of a model turn: decide whether the loop must execute
//! router-owned tool calls, hand the turn back to the caller, or stop.

use std::collections::BTreeSet;

use crate::language_model::types::Content;

/// One router-owned tool call extracted from a model turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterCall {
    /// Provider-assigned call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Raw JSON-encoded arguments.
    pub arguments: String,
}

/// What the loop should do after one upstream turn.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnDisposition {
    /// No router-owned calls — the turn is the final answer.
    Done,
    /// A client-owned tool call is present (mixed or pure-client): the whole
    /// turn returns to the caller, which executes its own tools. v1 does not
    /// partially execute a mixed turn.
    HandBack,
    /// Every tool call is router-owned — execute them and loop.
    Execute(Vec<RouterCall>),
}

/// Classify `content` against the set of router-owned tool names.
///
/// Provider-executed calls (`provider_executed: true`) were already run by the
/// upstream and are ignored. If any client-owned call is present the turn is
/// handed back; otherwise the router-owned calls (if any) are executed.
pub fn classify_turn(content: &[Content], owned: &BTreeSet<String>) -> TurnDisposition {
    let mut router_calls = Vec::new();
    let mut has_client_call = false;
    for block in content {
        if let Content::ToolCall {
            id,
            name,
            arguments,
            provider_executed,
            ..
        } = block
        {
            if *provider_executed {
                continue;
            }
            if owned.contains(name) {
                router_calls.push(RouterCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            } else {
                has_client_call = true;
            }
        }
    }
    if has_client_call {
        TurnDisposition::HandBack
    } else if router_calls.is_empty() {
        TurnDisposition::Done
    } else {
        TurnDisposition::Execute(router_calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::ProviderMetadata;

    fn call(name: &str, provider_executed: bool) -> Content {
        Content::ToolCall {
            id: format!("{name}-id"),
            name: name.to_string(),
            arguments: "{}".to_string(),
            provider_executed,
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    fn text() -> Content {
        Content::Text {
            text: "answer".to_string(),
            provider_metadata: ProviderMetadata::new(),
        }
    }

    fn owned(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn execute_when_all_calls_router_owned() {
        let d = classify_turn(&[call("search", false)], &owned(&["search"]));
        assert_eq!(
            d,
            TurnDisposition::Execute(vec![RouterCall {
                id: "search-id".to_string(),
                name: "search".to_string(),
                arguments: "{}".to_string(),
            }])
        );
    }

    #[test]
    fn handback_when_mixed_router_and_client() {
        let d = classify_turn(
            &[call("search", false), call("client_fn", false)],
            &owned(&["search"]),
        );
        assert_eq!(d, TurnDisposition::HandBack);
    }

    #[test]
    fn handback_when_only_client_call() {
        let d = classify_turn(&[call("client_fn", false)], &owned(&["search"]));
        assert_eq!(d, TurnDisposition::HandBack);
    }

    #[test]
    fn done_when_no_tool_calls() {
        let d = classify_turn(&[text()], &owned(&["search"]));
        assert_eq!(d, TurnDisposition::Done);
    }

    #[test]
    fn done_when_owned_but_provider_executed() {
        let d = classify_turn(&[call("search", true)], &owned(&["search"]));
        assert_eq!(d, TurnDisposition::Done);
    }
}
