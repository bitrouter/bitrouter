//! `NamespaceScopeExecutor` — MCP `Executor` decorator enforcing per-agent
//! Walrus memory namespace scoping (Strategy A).
//!
//! Sits at the innermost layer of the executor stack, below the aggregator and
//! cache, so every call it sees is a resolved `McpTarget::Direct` with the
//! upstream's bare tool name. Non-memory calls and unrestricted agents pass
//! through untouched; scoped agents have `namespace` injected or rejected.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use bitrouter_sdk::language_model::server_tools::toolset::AGENT_HEADER;
use bitrouter_sdk::mcp::{Executor, McpRequest, McpResponse, McpStreamPart, McpTarget};
use bitrouter_sdk::{BitrouterError, Result};

use crate::memory::config::{MemoryScopeTable, ScopeDecision};

/// Walrus tools that accept a `namespace` argument and so must be scoped.
///
/// This list is matched against the live relayer's tool set, not just the
/// public docs: `memwal_remember_bulk` is a namespaced write that is easy to
/// miss, and leaving it out lets a scoped agent write to any namespace. The
/// non-namespaced `memwal_health` is deliberately absent.
const NAMESPACED_TOOLS: &[&str] = &[
    "memwal_remember",
    "memwal_remember_bulk",
    "memwal_recall",
    "memwal_analyze",
    "memwal_restore",
];

/// Decorator that enforces namespace scoping over an inner [`Executor`].
pub struct NamespaceScopeExecutor<E: Executor> {
    inner: Arc<E>,
    table: MemoryScopeTable,
}

impl<E: Executor> NamespaceScopeExecutor<E> {
    /// Wrap `inner` with the scope `table`. An empty/disabled table is a pure
    /// passthrough.
    pub fn new(inner: Arc<E>, table: MemoryScopeTable) -> Self {
        Self { inner, table }
    }

    fn agent_of(request: &McpRequest) -> &str {
        request
            .headers
            .get(AGENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
    }

    fn is_memory_call(&self, target: &McpTarget, request: &McpRequest) -> bool {
        if !self.table.is_enabled() || request.method != "tools/call" {
            return false;
        }
        let on_memory_server = matches!(
            target,
            McpTarget::Direct { server_name, .. } if server_name == self.table.server()
        );
        let tool = request
            .params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        on_memory_server && NAMESPACED_TOOLS.contains(&tool)
    }

    /// Returns `Ok(Some(rewritten))` when the namespace was injected,
    /// `Ok(None)` to pass through unchanged, or `Err` when the call is denied.
    fn scope(&self, target: &McpTarget, request: &McpRequest) -> Result<Option<McpRequest>> {
        if !self.is_memory_call(target, request) {
            return Ok(None);
        }
        let agent = Self::agent_of(request);
        let requested = request
            .params
            .get("arguments")
            .and_then(|a| a.get("namespace"))
            .and_then(|v| v.as_str());
        match self.table.decide(agent, requested) {
            ScopeDecision::Passthrough => Ok(None),
            ScopeDecision::Inject(ns) => {
                let mut req = request.clone();
                let params = req.params.as_object_mut().ok_or_else(|| {
                    BitrouterError::bad_request(
                        "mcp memory: tools/call params must be a JSON object",
                    )
                })?;
                let args = params
                    .entry("arguments")
                    .or_insert_with(|| serde_json::json!({}));
                let obj = args.as_object_mut().ok_or_else(|| {
                    BitrouterError::bad_request(
                        "mcp memory: tools/call arguments must be a JSON object",
                    )
                })?;
                obj.insert("namespace".to_string(), ns.into());
                Ok(Some(req))
            }
            ScopeDecision::Deny { agent, requested } => Err(BitrouterError::Unauthorized(format!(
                "agent '{agent}' may not access memory namespace '{requested}'"
            ))),
        }
    }
}

#[async_trait]
impl<E: Executor + 'static> Executor for NamespaceScopeExecutor<E> {
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
        match self.scope(target, request)? {
            Some(rewritten) => self.inner.execute(target, &rewritten).await,
            None => self.inner.execute(target, request).await,
        }
    }

    async fn execute_streaming(
        &self,
        target: &McpTarget,
        request: &McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        match self.scope(target, request)? {
            Some(rewritten) => self.inner.execute_streaming(target, &rewritten).await,
            None => self.inner.execute_streaming(target, request).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::mcp::transport::McpTransport;

    use crate::memory::config::MemoryScopeConfig;

    /// Inner executor that records the request it was handed and returns a
    /// canned ok response.
    struct RecordingExecutor {
        last: Mutex<Option<McpRequest>>,
    }
    impl RecordingExecutor {
        fn new() -> Self {
            Self {
                last: Mutex::new(None),
            }
        }
        fn last_namespace(&self) -> Option<String> {
            self.last_arg("namespace")
        }
        fn last_arg(&self, key: &str) -> Option<String> {
            self.last
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.params.get("arguments"))
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
    }
    #[async_trait]
    impl Executor for RecordingExecutor {
        async fn execute(&self, _t: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
            *self.last.lock().unwrap() = Some(request.clone());
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: serde_json::json!({ "ok": true }),
            })
        }
    }

    fn table() -> MemoryScopeTable {
        let cfg: MemoryScopeConfig = serde_json::from_value(serde_json::json!({
            "server": "memory",
            "default_namespace": "shared",
            "agents": {
                "orchestrator": { "namespaces": ["*"] },
                "researcher":   { "namespaces": ["research"], "default": "research" }
            }
        }))
        .unwrap();
        MemoryScopeTable::from_config(&cfg)
    }

    fn memory_target() -> McpTarget {
        McpTarget::Direct {
            server_name: "memory".into(),
            transport: McpTransport::Http {
                url: "https://relayer.example/api/mcp".into(),
                headers: Default::default(),
            },
        }
    }

    fn call(agent: Option<&str>, tool: &str, namespace: Option<&str>) -> McpRequest {
        let mut args = serde_json::Map::new();
        if let Some(ns) = namespace {
            args.insert("namespace".into(), ns.into());
        }
        let params = serde_json::json!({ "name": tool, "arguments": args });
        let mut headers = http::HeaderMap::new();
        if let Some(a) = agent {
            headers.insert("x-bitrouter-agent", a.parse().unwrap());
        }
        McpRequest::direct("memory", "tools/call", params, CallerContext::new("k", "u"))
            .with_headers(headers)
    }

    async fn run(req: McpRequest) -> (Arc<RecordingExecutor>, Result<McpResponse>) {
        let inner = Arc::new(RecordingExecutor::new());
        let exec = NamespaceScopeExecutor::new(inner.clone(), table());
        let res = exec.execute(&memory_target(), &req).await;
        (inner, res)
    }

    #[tokio::test]
    async fn allowed_namespace_passes_through_unchanged() {
        let (inner, res) = run(call(Some("researcher"), "memwal_recall", Some("research"))).await;
        assert!(res.is_ok());
        assert_eq!(inner.last_namespace().as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn disallowed_namespace_is_rejected() {
        let (_inner, res) = run(call(Some("researcher"), "memwal_recall", Some("secret"))).await;
        let err = res.unwrap_err();
        assert_eq!(err.status(), 401);
    }

    #[tokio::test]
    async fn omitted_namespace_gets_agent_default() {
        let (inner, res) = run(call(Some("researcher"), "memwal_remember", None)).await;
        assert!(res.is_ok());
        assert_eq!(inner.last_namespace().as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn remember_bulk_is_scoped() {
        // `memwal_remember_bulk` is a namespaced write tool present on the live
        // relayer but easy to miss from the docs — it must be guarded like
        // `memwal_remember`, or a scoped agent could write to any namespace.
        let (_inner, denied) = run(call(
            Some("researcher"),
            "memwal_remember_bulk",
            Some("secret"),
        ))
        .await;
        assert_eq!(denied.unwrap_err().status(), 401);
        let (inner, ok) = run(call(Some("researcher"), "memwal_remember_bulk", None)).await;
        assert!(ok.is_ok());
        assert_eq!(inner.last_namespace().as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn unrestricted_agent_is_untouched() {
        let (inner, res) = run(call(
            Some("orchestrator"),
            "memwal_recall",
            Some("anything"),
        ))
        .await;
        assert!(res.is_ok());
        assert_eq!(inner.last_namespace().as_deref(), Some("anything"));
    }

    #[tokio::test]
    async fn unknown_agent_naming_namespace_is_rejected() {
        let (_inner, res) = run(call(None, "memwal_recall", Some("research"))).await;
        assert_eq!(res.unwrap_err().status(), 401);
    }

    #[tokio::test]
    async fn unknown_agent_omitting_namespace_gets_global_default() {
        let (inner, res) = run(call(None, "memwal_recall", None)).await;
        assert!(res.is_ok());
        assert_eq!(inner.last_namespace().as_deref(), Some("shared"));
    }

    #[tokio::test]
    async fn non_namespaced_method_passes_through() {
        // `tools/list` is not a tools/call — never scoped.
        let inner = Arc::new(RecordingExecutor::new());
        let exec = NamespaceScopeExecutor::new(inner.clone(), table());
        let req = McpRequest::direct(
            "memory",
            "tools/list",
            serde_json::json!({}),
            CallerContext::new("k", "u"),
        );
        assert!(exec.execute(&memory_target(), &req).await.is_ok());
    }

    #[tokio::test]
    async fn inject_into_non_object_arguments_is_rejected() {
        // `researcher` omits namespace (Inject path) but `arguments` is not an
        // object — injection must fail closed rather than pass through unscoped.
        let mut headers = http::HeaderMap::new();
        headers.insert("x-bitrouter-agent", "researcher".parse().unwrap());
        let req = McpRequest::direct(
            "memory",
            "tools/call",
            serde_json::json!({ "name": "memwal_recall", "arguments": "oops" }),
            CallerContext::new("k", "u"),
        )
        .with_headers(headers);
        let (_inner, res) = {
            let inner = Arc::new(RecordingExecutor::new());
            let exec = NamespaceScopeExecutor::new(inner.clone(), table());
            let res = exec.execute(&memory_target(), &req).await;
            (inner, res)
        };
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn inject_preserves_other_arguments() {
        // `researcher` omits namespace but passes another argument; injecting the
        // default namespace must not drop the existing argument.
        let mut headers = http::HeaderMap::new();
        headers.insert("x-bitrouter-agent", "researcher".parse().unwrap());
        let req = McpRequest::direct(
            "memory",
            "tools/call",
            serde_json::json!({ "name": "memwal_recall", "arguments": { "query": "hi" } }),
            CallerContext::new("k", "u"),
        )
        .with_headers(headers);
        let inner = Arc::new(RecordingExecutor::new());
        let exec = NamespaceScopeExecutor::new(inner.clone(), table());
        assert!(exec.execute(&memory_target(), &req).await.is_ok());
        assert_eq!(inner.last_arg("query").as_deref(), Some("hi"));
        assert_eq!(inner.last_namespace().as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn other_server_passes_through() {
        let inner = Arc::new(RecordingExecutor::new());
        let exec = NamespaceScopeExecutor::new(inner.clone(), table());
        let target = McpTarget::Direct {
            server_name: "not-memory".into(),
            transport: McpTransport::Http {
                url: "https://other.example/mcp".into(),
                headers: Default::default(),
            },
        };
        // researcher naming a forbidden namespace, but not on the memory server.
        let req = call(Some("researcher"), "memwal_recall", Some("secret"));
        assert!(exec.execute(&target, &req).await.is_ok());
    }
}
