//! Aggregating [`Executor`] — fan-out across many upstream MCP servers behind
//! one virtual `POST /mcp` endpoint.
//!
//! Wraps an inner [`Executor`] that only knows how to handle
//! [`McpTarget::Direct`]; this layer is responsible for resolving
//! [`McpTarget::Aggregate`] into N direct calls, merging their results, and
//! applying the per-server `tool_prefix` rewrites the MCP-gateway ecosystem
//! has converged on (MetaMCP, mcphub, Pluggedin, Docker MCP Gateway,
//! Cloudflare portals).
//!
//! Per-method behaviour (see issue #483 for the dispatch table):
//!
//! | Inbound | Behaviour |
//! |---|---|
//! | `tools/list` | fan-out → concat tools, prepend `tool_prefix` to each name |
//! | `resources/list` | fan-out → concat (no prefix, URIs are globally addressable) |
//! | `resources/templates/list` | fan-out → concat |
//! | `prompts/list` | fan-out → concat, prepend `tool_prefix` to each name |
//! | `tools/call` | strip prefix from `params.name`, dispatch to owning member |
//! | `resources/read` | try each member, first success wins |
//! | `prompts/get` | strip prefix from `params.name`, dispatch to owning member |
//!
//! Failure semantics for fan-out: **partial-success** by default. Servers that
//! responded contribute their results; servers that failed are listed under
//! `result._bitrouterErrors = [{server, error}]`. The `_bitrouter` prefix
//! namespaces gateway-injected fields so they cannot collide with anything the
//! upstream method's result schema may carry now or later.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};

use super::{
    AggregateMember, Executor, McpRequest, McpResponse, McpStreamPart, McpTarget, ServerSelector,
};
use crate::error::{BitrouterError, Result};

/// Fan-out wrapper over an inner [`Executor`]. Passes [`McpTarget::Direct`]
/// straight through to the inner; handles [`McpTarget::Aggregate`] by issuing
/// per-member direct calls and merging the results.
pub struct AggregatingExecutor<E: Executor> {
    inner: Arc<E>,
}

impl<E: Executor> AggregatingExecutor<E> {
    /// Wrap an inner executor.
    pub fn new(inner: Arc<E>) -> Self {
        Self { inner }
    }

    /// Build a per-member direct request from the aggregate request. Used by
    /// fan-out and prefix-routed methods.
    fn direct_request(request: &McpRequest, member: &AggregateMember) -> McpRequest {
        // The aggregate's `request_id` is the inbound id from the JSON-RPC
        // client. Per-member sub-calls reuse it so settled/observe hooks can
        // correlate the whole fan-out as one logical request.
        McpRequest {
            request_id: request.request_id.clone(),
            selector: ServerSelector::Direct(member.server_name.clone()),
            method: request.method.clone(),
            params: request.params.clone(),
            caller: request.caller.clone(),
        }
    }

    fn direct_target(member: &AggregateMember) -> McpTarget {
        McpTarget::Direct {
            server_name: member.server_name.clone(),
            transport: member.transport.clone(),
        }
    }

    async fn fanout_list(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
        list_key: &str,
        prefix_field: Option<&str>,
    ) -> Result<McpResponse> {
        // Fan out concurrently — cold-cache latency is Σ→max(per-server). Errors
        // are collected as data (partial-success semantics), so there is no
        // short-circuit and `join_all` is correct here. Order of members is
        // preserved in the merged result because `join_all` returns results in
        // input order regardless of completion order.
        let calls = members.iter().map(|member| {
            let sub_req = Self::direct_request(request, member);
            let target = Self::direct_target(member);
            async move { (member, self.inner.execute(&target, &sub_req).await) }
        });
        let outcomes = futures::future::join_all(calls).await;

        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut errors: Vec<serde_json::Value> = Vec::new();
        for (member, outcome) in outcomes {
            match outcome {
                Ok(resp) => match resp.result.get(list_key).and_then(|v| v.as_array()) {
                    Some(arr) => {
                        for entry in arr {
                            let mut entry = entry.clone();
                            if let Some(field) = prefix_field
                                && let Some(obj) = entry.as_object_mut()
                                && let Some(name) = obj.get_mut(field).and_then(|v| v.as_str())
                            {
                                let prefixed = format!("{}{name}", member.tool_prefix);
                                obj.insert(field.to_string(), prefixed.into());
                            }
                            items.push(entry);
                        }
                    }
                    None => errors.push(serde_json::json!({
                        "server": member.server_name,
                        "error": format!(
                            "upstream response missing or non-array '{list_key}'",
                        ),
                    })),
                },
                Err(e) => errors.push(serde_json::json!({
                    "server": member.server_name,
                    "error": e.to_string(),
                })),
            }
        }
        let mut result = serde_json::json!({ list_key: items });
        if !errors.is_empty() {
            result["_bitrouterErrors"] = serde_json::Value::Array(errors);
        }
        Ok(McpResponse {
            request_id: request.request_id.clone(),
            result,
        })
    }

    /// Resolve a prefix-routed request (`tools/call` / `prompts/get`) into the
    /// per-member direct request and target. Returns the rewritten `name`
    /// stripped of its prefix.
    ///
    /// Longest-prefix wins so `a__` does not steal calls intended for `ab__`
    /// when both servers are registered.
    fn resolve_prefixed(
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<(McpRequest, McpTarget)> {
        let name = request
            .params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "mcp aggregate {}: params.name is required",
                    request.method
                ))
            })?;
        let (member, stripped) = members
            .iter()
            .filter_map(|m| name.strip_prefix(&m.tool_prefix).map(|s| (m, s)))
            .max_by_key(|(m, _)| m.tool_prefix.len())
            .ok_or_else(|| {
                BitrouterError::NotFound(format!(
                    "mcp aggregate {}: no member prefix matches '{name}'",
                    request.method
                ))
            })?;
        let mut sub_req = Self::direct_request(request, member);
        if let Some(obj) = sub_req.params.as_object_mut() {
            obj.insert("name".to_string(), stripped.to_string().into());
        }
        Ok((sub_req, Self::direct_target(member)))
    }

    /// Strip the `tool_prefix` from `params.name` and dispatch to the owning
    /// member. Used by `tools/call` and `prompts/get`.
    async fn prefixed_dispatch(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        let (sub_req, target) = Self::resolve_prefixed(members, request)?;
        self.inner.execute(&target, &sub_req).await
    }

    /// `resources/read` — try each member; first success wins. Errors are
    /// accumulated and returned only if every member fails.
    async fn try_each(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        let mut errors = Vec::new();
        for member in members {
            let sub_req = Self::direct_request(request, member);
            let target = Self::direct_target(member);
            match self.inner.execute(&target, &sub_req).await {
                Ok(resp) => return Ok(resp),
                Err(e) => errors.push(format!("{}: {}", member.server_name, e)),
            }
        }
        Err(BitrouterError::NotFound(format!(
            "mcp aggregate {}: no member served the request ({})",
            request.method,
            errors.join("; ")
        )))
    }

    async fn dispatch_aggregate(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        if members.is_empty() {
            // Empty aggregate (e.g. every server set `aggregate: false`) —
            // return an empty list shape so list calls keep parsing on the
            // client. Non-list methods surface as "method not found".
            return match request.method.as_str() {
                "tools/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "tools": [] }),
                }),
                "resources/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "resources": [] }),
                }),
                "resources/templates/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "resourceTemplates": [] }),
                }),
                "prompts/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "prompts": [] }),
                }),
                other => Err(BitrouterError::NotFound(format!(
                    "mcp aggregate {other}: no member servers configured"
                ))),
            };
        }
        match request.method.as_str() {
            "tools/list" => {
                self.fanout_list(members, request, "tools", Some("name"))
                    .await
            }
            "resources/list" => self.fanout_list(members, request, "resources", None).await,
            "resources/templates/list" => {
                self.fanout_list(members, request, "resourceTemplates", None)
                    .await
            }
            "prompts/list" => {
                self.fanout_list(members, request, "prompts", Some("name"))
                    .await
            }
            "tools/call" | "prompts/get" => self.prefixed_dispatch(members, request).await,
            "resources/read" => self.try_each(members, request).await,
            other => Err(BitrouterError::NotFound(format!(
                "mcp aggregate {other}: not supported on the aggregate endpoint"
            ))),
        }
    }
}

#[async_trait]
impl<E: Executor + 'static> Executor for AggregatingExecutor<E> {
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
        match target {
            McpTarget::Direct { .. } => self.inner.execute(target, request).await,
            McpTarget::Aggregate { members } => self.dispatch_aggregate(members, request).await,
        }
    }

    async fn execute_streaming(
        &self,
        target: &McpTarget,
        request: &McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        match target {
            McpTarget::Direct { .. } => self.inner.execute_streaming(target, request).await,
            McpTarget::Aggregate { members } => {
                // `tools/call` (and `prompts/get`) is the only aggregate-mode
                // method that meaningfully streams — it routes to a single
                // member by prefix, so the inner stream passes through.
                if matches!(request.method.as_str(), "tools/call" | "prompts/get") {
                    let (sub_req, target) = Self::resolve_prefixed(members, request)?;
                    return self.inner.execute_streaming(&target, &sub_req).await;
                }
                // For fan-out methods (list-shaped, `resources/read`), there
                // is no meaningful intermediate stream — buffer the merged
                // response and emit a single `Final`.
                let response = self.dispatch_aggregate(members, request).await?;
                Ok(stream::once(async move { Ok(McpStreamPart::Final(response)) }).boxed())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::mcp::transport::McpTransport;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct CannedExecutor {
        responses: Mutex<HashMap<String, Result<serde_json::Value>>>,
    }

    impl CannedExecutor {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
            }
        }
        fn with(self, server: &str, value: serde_json::Value) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(server.to_string(), Ok(value));
            self
        }
        fn with_err(self, server: &str, err: BitrouterError) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(server.to_string(), Err(err));
            self
        }
    }

    #[async_trait]
    impl Executor for CannedExecutor {
        async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
            let name = match target {
                McpTarget::Direct { server_name, .. } => server_name.clone(),
                McpTarget::Aggregate { .. } => panic!("inner saw aggregate"),
            };
            let mut map = self.responses.lock().unwrap();
            // Allow per-server multi-method canned responses by keying on
            // "{server}:{method}" when the test prepared one, falling back to
            // the bare server key.
            let composite = format!("{name}:{}", request.method);
            let entry = map
                .remove(&composite)
                .or_else(|| map.remove(&name))
                .unwrap_or_else(|| {
                    Err(BitrouterError::internal(format!(
                        "no canned response for '{name}' / '{composite}'"
                    )))
                });
            entry.map(|result| McpResponse {
                request_id: request.request_id.clone(),
                result,
            })
        }
    }

    fn member(name: &str) -> AggregateMember {
        AggregateMember {
            server_name: name.into(),
            tool_prefix: format!("{name}__"),
            transport: McpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: Default::default(),
            },
        }
    }

    fn agg_req(method: &str, params: serde_json::Value) -> McpRequest {
        McpRequest::aggregate(method, params, CallerContext::new("k", "u"))
    }

    #[tokio::test]
    async fn tools_list_fanout_prefixes_names_and_concats() {
        let inner = CannedExecutor::new()
            .with(
                "a",
                serde_json::json!({"tools": [{"name": "search"}, {"name": "fetch"}]}),
            )
            .with("b", serde_json::json!({"tools": [{"name": "noop"}]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("tools/list", serde_json::json!({})))
            .await
            .unwrap();
        let names: Vec<String> = resp.result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a__search", "a__fetch", "b__noop"]);
        assert!(resp.result.get("_bitrouterErrors").is_none());
    }

    #[tokio::test]
    async fn tools_list_partial_failure_surfaces_under_errors() {
        let inner = CannedExecutor::new()
            .with("a", serde_json::json!({"tools": [{"name": "ok"}]}))
            .with_err(
                "b",
                BitrouterError::Upstream {
                    status: 502,
                    message: "boom".into(),
                },
            );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("tools/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.result["tools"][0]["name"], "a__ok");
        let errors = resp.result["_bitrouterErrors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["server"], "b");
    }

    #[tokio::test]
    async fn tools_call_strips_prefix_and_dispatches_to_member() {
        let inner = CannedExecutor::new().with("a", serde_json::json!({"ok": true}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "tools/call",
                    serde_json::json!({ "name": "a__search", "arguments": {} }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["ok"], true);
    }

    #[tokio::test]
    async fn tools_call_uses_longest_matching_prefix() {
        // Two servers — "a" with prefix "a__" and "ab" with prefix "ab__". A
        // call to "ab__tool" must route to "ab" (longer prefix) even though
        // "a__" is also a valid `strip_prefix` candidate. Without
        // longest-prefix-wins this would silently misroute to "a" with the
        // stripped name "b__tool".
        let inner = CannedExecutor::new()
            .with("ab", serde_json::json!({"server": "ab"}))
            .with("a", serde_json::json!({"server": "a"}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let mut m_a = member("a");
        m_a.tool_prefix = "a__".into();
        let mut m_ab = member("ab");
        m_ab.tool_prefix = "ab__".into();
        let target = McpTarget::Aggregate {
            members: vec![m_a, m_ab],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "tools/call",
                    serde_json::json!({ "name": "ab__tool", "arguments": {} }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["server"], "ab");
    }

    #[tokio::test]
    async fn tools_call_unknown_prefix_is_404() {
        let inner = CannedExecutor::new();
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        let err = exec
            .execute(
                &target,
                &agg_req("tools/call", serde_json::json!({ "name": "ghost__search" })),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn resources_read_returns_first_success() {
        let inner = CannedExecutor::new()
            .with_err("a", BitrouterError::NotFound("missing".into()))
            .with("b", serde_json::json!({"contents": ["data"]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req("resources/read", serde_json::json!({ "uri": "x://y" })),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["contents"][0], "data");
    }

    #[tokio::test]
    async fn resources_read_all_failures_is_404() {
        let inner = CannedExecutor::new()
            .with_err("a", BitrouterError::NotFound("missing a".into()))
            .with_err("b", BitrouterError::NotFound("missing b".into()));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let err = exec
            .execute(
                &target,
                &agg_req("resources/read", serde_json::json!({ "uri": "x://y" })),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn empty_aggregate_returns_empty_list_for_list_methods() {
        let inner = CannedExecutor::new();
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate { members: vec![] };
        let resp = exec
            .execute(&target, &agg_req("tools/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.result["tools"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn direct_target_passes_through_to_inner() {
        let inner = CannedExecutor::new().with("only", serde_json::json!({"pass": true}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Direct {
            server_name: "only".into(),
            transport: McpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: Default::default(),
            },
        };
        let resp = exec
            .execute(
                &target,
                &McpRequest::direct(
                    "only",
                    "tools/list",
                    serde_json::json!({}),
                    CallerContext::new("k", "u"),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["pass"], true);
    }
}
