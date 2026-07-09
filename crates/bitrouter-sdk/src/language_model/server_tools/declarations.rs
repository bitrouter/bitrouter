//! Server-tool declarations: how a caller turns on the advisor / sub-agent /
//! fusion tools, parsed once and stashed for the toolsets to read back.
//!
//! A caller declares a server tool by putting a provider-defined tool in the
//! request `tools` array whose name resolves to `advisor` / `subagent` /
//! `fusion` (bare, or namespaced: `bitrouter:advisor`, `bitrouter.fusion`, …).
//! Its config rides the tool's `args` (tolerating an OpenRouter-style
//! `parameters` wrapper). The toolsets receive only a [`ToolContext`] (caller +
//! metadata), not the prompt, so [`ServerToolDeclarationsHook`] parses these
//! once on every request and stashes the result on the request context under
//! [`declarations_plugin_id`]; each toolset reads back its own slice. Pure
//! observation — the hook never denies.

use std::sync::OnceLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::fusion::config::FusionConfig;
use super::toolset::ToolContext;
use crate::error::Result;
use crate::language_model::context::PipelineContext;
use crate::language_model::hooks::{HookDecision, PreRequestHook};
use crate::language_model::types::{Prompt, ProviderMetadata, Tool};
use crate::plugin::PluginId;

/// Router-tool name the model calls to consult a stronger model.
pub const ADVISOR_TOOL: &str = "advisor";
/// Router-tool name the model calls to delegate a task to a worker model.
pub const SUBAGENT_TOOL: &str = "subagent";
/// Router-tool name the model calls to search the web.
pub const WEB_SEARCH_TOOL: &str = "web_search";
/// Router-tool name the model calls to fetch a URL's content.
pub const WEB_FETCH_TOOL: &str = "web_fetch";

/// Plugin id under which [`ServerToolDeclarations`] is stashed on the request
/// context by the pre-request hook, for the toolsets to read back.
pub fn declarations_plugin_id() -> &'static PluginId {
    static ID: OnceLock<PluginId> = OnceLock::new();
    ID.get_or_init(|| PluginId::new("bitrouter:server-tool-declarations"))
}

/// Per-request Advisor config. `model`/`instructions` pin the advisor; an absent
/// `model` falls back to the outer request model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AdvisorConfig {
    /// Pinned advisor model (else the call-arg override, else the parent model).
    pub model: Option<String>,
    /// System instructions for the advisor.
    pub instructions: Option<String>,
    /// Server tools the advisor may use, in provider-namespaced declaration
    /// form; forwarded to the nested completion (see [`forwarded_tools`]).
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

/// Per-request Web-search config. Both fields are optional overrides: `backend`
/// pins one of the configured search backends by name (else the default), and
/// `max_results` lowers the per-call result cap. The query itself rides the
/// tool call's arguments, not the declaration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebSearchDeclaration {
    /// Pin a configured backend by name (else the deployment default).
    pub backend: Option<String>,
    /// Cap on results for this request (else the deployment default).
    pub max_results: Option<u32>,
}

/// Per-request Web-fetch config. Both fields are optional overrides: `backend`
/// pins one of the configured fetch backends by name (else the default), and
/// `max_content_tokens` lowers the per-call content cap. The URL itself rides the
/// tool call's arguments, not the declaration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebFetchDeclaration {
    /// Pin a configured backend by name (else the deployment default).
    pub backend: Option<String>,
    /// Cap on returned content for this request, in tokens (else the deployment
    /// default).
    pub max_content_tokens: Option<u32>,
}

/// Per-request Sub-agent config (worker model + instructions + tools).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubAgentConfig {
    /// Pinned worker model (else the parent model).
    pub model: Option<String>,
    /// System instructions for the worker.
    pub instructions: Option<String>,
    /// Server tools the worker may use (see [`AdvisorConfig::tools`]).
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

/// Every server-tool declaration parsed from one request, plus the outer model
/// used as the default nested model when none is pinned.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerToolDeclarations {
    /// The advisor declaration, if any.
    pub advisor: Option<AdvisorConfig>,
    /// The sub-agent declaration, if any.
    pub subagent: Option<SubAgentConfig>,
    /// The fusion declaration, if any (already resolved against `parent_model`).
    pub fusion: Option<FusionConfig>,
    /// The web-search declaration, if any.
    pub web_search: Option<WebSearchDeclaration>,
    /// The web-fetch declaration, if any.
    pub web_fetch: Option<WebFetchDeclaration>,
    /// The outer request model — the default nested model.
    pub parent_model: String,
}

impl ServerToolDeclarations {
    /// Parse the advisor / sub-agent / fusion declarations off a prompt's tools.
    pub fn from_prompt(prompt: &Prompt) -> Self {
        let mut decls = Self {
            parent_model: prompt.model.clone(),
            ..Self::default()
        };
        for tool in &prompt.tools {
            // Fusion resolves its panel/judge against the parent model at parse
            // time, so try it first; it returns `None` for non-fusion tools.
            if let Some(fusion) = FusionConfig::from_tool(tool, &prompt.model) {
                decls.fusion = Some(fusion);
                continue;
            }
            let Tool::ProviderDefined { name, args, .. } = tool else {
                continue;
            };
            match server_tool_kind(name) {
                Some(Kind::Advisor) => {
                    decls.advisor = Some(AdvisorConfig {
                        model: str_field(args, "model"),
                        instructions: str_field(args, "instructions"),
                        tools: array_field(args, "tools"),
                    });
                }
                Some(Kind::SubAgent) => {
                    decls.subagent = Some(SubAgentConfig {
                        model: str_field(args, "model"),
                        instructions: str_field(args, "instructions"),
                        tools: array_field(args, "tools"),
                    });
                }
                Some(Kind::WebSearch) => {
                    decls.web_search = Some(WebSearchDeclaration {
                        backend: str_field(args, "backend"),
                        max_results: u32_field(args, "max_results"),
                    });
                }
                Some(Kind::WebFetch) => {
                    decls.web_fetch = Some(WebFetchDeclaration {
                        backend: str_field(args, "backend"),
                        max_content_tokens: u32_field(args, "max_content_tokens"),
                    });
                }
                None => {}
            }
        }
        decls
    }

    /// Read the parsed declarations off a request context (the pre-request hook
    /// stashed them under [`declarations_plugin_id`]).
    pub fn from_context(ctx: &ToolContext) -> Option<Self> {
        ctx.get_metadata(declarations_plugin_id())
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Whether the request declared no server tool.
    pub fn is_empty(&self) -> bool {
        self.advisor.is_none()
            && self.subagent.is_none()
            && self.fusion.is_none()
            && self.web_search.is_none()
            && self.web_fetch.is_none()
    }
}

enum Kind {
    Advisor,
    SubAgent,
    WebSearch,
    WebFetch,
}

/// Recognise an advisor / sub-agent / web-search declaration by tool name: the
/// bare name or a namespaced form whose final `:`/`.` segment is the name.
///
/// `web_search` is the exception. Unlike the bitrouter-invented
/// `advisor` / `subagent` / `fusion` names, `web_search` is also a real
/// *native* tool name on several providers (e.g. OpenAI's Responses
/// `web_search`). Matching it bare would hijack a caller's genuine native tool,
/// so it is recognised only under the explicit `bitrouter` namespace
/// (`{"type":"bitrouter:web_search"}`, the documented declaration form); a bare
/// or foreign-namespaced `web_search` is left untouched for the upstream.
fn server_tool_kind(name: &str) -> Option<Kind> {
    match name.rsplit([':', '.']).next().unwrap_or(name) {
        ADVISOR_TOOL => Some(Kind::Advisor),
        SUBAGENT_TOOL => Some(Kind::SubAgent),
        WEB_SEARCH_TOOL if is_bitrouter_namespaced(name) => Some(Kind::WebSearch),
        WEB_FETCH_TOOL if is_bitrouter_namespaced(name) => Some(Kind::WebFetch),
        _ => None,
    }
}

/// Whether `name` carries the explicit `bitrouter:` / `bitrouter.` namespace —
/// the documented `{"type":"bitrouter:<tool>"}` declaration form, as opposed to
/// a bare or foreign-namespaced tool a provider defines itself. The inbound
/// decoders keep the namespace in the canonical tool `name` (a typeless tool
/// defaults its `name` to the full `type`), so the prefix is visible here.
fn is_bitrouter_namespaced(name: &str) -> bool {
    name.split_once([':', '.'])
        .is_some_and(|(namespace, _)| namespace == "bitrouter")
}

/// Read a string field from a config object, tolerating an OpenRouter-style
/// `parameters` wrapper.
fn str_field(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .or_else(|| args.get("parameters").and_then(|p| p.get(key)))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Read a `u32` field from a config object (same `parameters`-wrapper tolerance
/// as [`str_field`]).
fn u32_field(args: &serde_json::Value, key: &str) -> Option<u32> {
    args.get(key)
        .or_else(|| args.get("parameters").and_then(|p| p.get(key)))
        .and_then(|v| v.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32)
}

/// Read an array field from a config object (same `parameters`-wrapper tolerance
/// as [`str_field`]).
fn array_field(args: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    args.get(key)
        .or_else(|| args.get("parameters").and_then(|p| p.get(key)))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Pre-request hook that stashes the parsed server-tool declarations on the
/// request context for the toolsets to read.
pub struct ServerToolDeclarationsHook;

#[async_trait]
impl PreRequestHook for ServerToolDeclarationsHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let decls = ServerToolDeclarations::from_prompt(ctx.prompt());
        if !decls.is_empty()
            && let Ok(value) = serde_json::to_value(&decls)
        {
            ctx.set_metadata(declarations_plugin_id(), value);
        }
        Ok(HookDecision::Allow)
    }
}

/// Convert a worker's declared server-tool specs into canonical IR tools to
/// forward into its nested completion. Each spec `{type, name?, …config}` is a
/// provider-defined (server) tool whose `type` is provider-namespaced
/// (`<provider>:<tool>` or `<provider>.<tool>`); it renders back to the nested
/// upstream's native shape via the SDK's `provider_defined_native`. Specs
/// without a string `type` are skipped.
pub fn forwarded_tools(specs: &[serde_json::Value]) -> Vec<Tool> {
    specs.iter().filter_map(spec_to_tool).collect()
}

fn spec_to_tool(spec: &serde_json::Value) -> Option<Tool> {
    let obj = spec.as_object()?;
    let ty = obj.get("type").and_then(|v| v.as_str())?;
    // Canonical provider-defined id is `<provider>.<tool>`; accept the `:`
    // namespacing the declarations use and normalise the first separator.
    let id = ty.replacen(':', ".", 1);
    let tail = id.rsplit('.').next().unwrap_or(&id);
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(tail)
        .to_string();
    let mut args = obj.clone();
    args.remove("type");
    args.remove("name");
    Some(Tool::ProviderDefined {
        id,
        name,
        args: serde_json::Value::Object(args),
        provider_metadata: ProviderMetadata::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::types::{GenerationParams, PipelineRequest};

    fn prompt_with(tools: Vec<Tool>) -> Prompt {
        Prompt {
            model: "anthropic/claude-opus-4.8".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools,
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn provider_tool(name: &str, args: serde_json::Value) -> Tool {
        Tool::ProviderDefined {
            id: format!("bitrouter.{name}"),
            name: name.to_string(),
            args,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    #[test]
    fn parses_advisor_subagent_and_fusion() {
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![
            provider_tool(
                "advisor",
                serde_json::json!({ "model": "anthropic/claude-opus-4.8", "instructions": "be critical" }),
            ),
            provider_tool(
                "bitrouter:subagent",
                serde_json::json!({ "model": "anthropic/claude-haiku-4.5" }),
            ),
            provider_tool(
                "fusion",
                serde_json::json!({ "panel": [{"model": "a/1"}, {"model": "b/2"}] }),
            ),
        ]));
        assert_eq!(
            decls.advisor,
            Some(AdvisorConfig {
                model: Some("anthropic/claude-opus-4.8".to_string()),
                instructions: Some("be critical".to_string()),
                tools: Vec::new(),
            })
        );
        assert_eq!(
            decls.subagent.as_ref().and_then(|s| s.model.as_deref()),
            Some("anthropic/claude-haiku-4.5")
        );
        assert_eq!(decls.fusion.as_ref().expect("fusion parsed").panel.len(), 2);
        assert_eq!(decls.parent_model, "anthropic/claude-opus-4.8");
        assert!(!decls.is_empty());
    }

    #[test]
    fn parses_web_search_declaration() {
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "bitrouter:web_search",
            serde_json::json!({ "backend": "exa", "max_results": 3 }),
        )]));
        assert!(!decls.is_empty());
        let ws = decls.web_search.expect("web_search parsed");
        assert_eq!(ws.backend.as_deref(), Some("exa"));
        assert_eq!(ws.max_results, Some(3));
    }

    #[test]
    fn bare_web_search_is_a_native_tool_not_a_declaration() {
        // A provider's own native `web_search` (no `bitrouter` namespace) must
        // NOT be taken over by the built-in tool — otherwise the caller silently
        // loses their model's native search.
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "web_search",
            serde_json::json!({}),
        )]));
        assert!(decls.web_search.is_none());
        assert!(decls.is_empty());
    }

    #[test]
    fn foreign_namespaced_web_search_is_not_a_declaration() {
        // Only the explicit `bitrouter` namespace declares the built-in tool;
        // another provider's namespaced `web_search` is left for the upstream.
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "openai:web_search",
            serde_json::json!({}),
        )]));
        assert!(decls.web_search.is_none());
        assert!(decls.is_empty());
    }

    #[test]
    fn ignores_function_and_unrelated_tools() {
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![
            Tool::Function {
                name: "advisor".to_string(),
                description: None,
                parameters: serde_json::json!({}),
                strict: None,
                provider_metadata: ProviderMetadata::new(),
            },
            provider_tool("code_interpreter", serde_json::json!({})),
        ]));
        assert!(decls.is_empty());
    }

    #[test]
    fn tolerates_parameters_wrapper() {
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "advisor",
            serde_json::json!({ "parameters": { "model": "m1" } }),
        )]));
        assert_eq!(decls.advisor.and_then(|a| a.model), Some("m1".to_string()));
    }

    #[test]
    fn forwards_worker_tools() {
        let forwarded = forwarded_tools(&[serde_json::json!({
            "type": "anthropic:web_search_20250305", "name": "web_search", "max_uses": 3
        })]);
        assert_eq!(forwarded.len(), 1);
        let Tool::ProviderDefined { id, name, args, .. } = &forwarded[0] else {
            panic!("expected a provider-defined tool");
        };
        assert_eq!(id, "anthropic.web_search_20250305");
        assert_eq!(name, "web_search");
        assert_eq!(args["max_uses"], 3);
        assert!(args.get("type").is_none() && args.get("name").is_none());
    }

    #[tokio::test]
    async fn hook_stashes_declarations_when_present() {
        let prompt = prompt_with(vec![provider_tool(
            "advisor",
            serde_json::json!({ "model": "m1" }),
        )]);
        let mut ctx =
            PipelineContext::new(PipelineRequest::new("m", CallerContext::local(), prompt));
        let decision = ServerToolDeclarationsHook.check(&mut ctx).await.unwrap();
        assert!(matches!(decision, HookDecision::Allow));
        assert!(ctx.get_metadata(declarations_plugin_id()).is_some());
    }

    #[tokio::test]
    async fn hook_no_stash_when_absent() {
        let prompt = prompt_with(Vec::new());
        let mut ctx =
            PipelineContext::new(PipelineRequest::new("m", CallerContext::local(), prompt));
        ServerToolDeclarationsHook.check(&mut ctx).await.unwrap();
        assert!(ctx.get_metadata(declarations_plugin_id()).is_none());
    }

    #[test]
    fn parses_web_fetch_declaration() {
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "bitrouter:web_fetch",
            serde_json::json!({ "backend": "exa", "max_content_tokens": 2000 }),
        )]));
        assert!(!decls.is_empty());
        let wf = decls.web_fetch.expect("web_fetch parsed");
        assert_eq!(wf.backend.as_deref(), Some("exa"));
        assert_eq!(wf.max_content_tokens, Some(2000));
    }

    #[test]
    fn bare_web_fetch_is_not_a_declaration() {
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "web_fetch",
            serde_json::json!({}),
        )]));
        assert!(decls.web_fetch.is_none());
        assert!(decls.is_empty());
    }

    #[test]
    fn foreign_namespaced_web_fetch_is_not_a_declaration() {
        // Only the explicit `bitrouter` namespace declares the built-in tool;
        // another provider's namespaced `web_fetch` is left for the upstream.
        let decls = ServerToolDeclarations::from_prompt(&prompt_with(vec![provider_tool(
            "openai:web_fetch",
            serde_json::json!({}),
        )]));
        assert!(decls.web_fetch.is_none());
        assert!(decls.is_empty());
    }
}
