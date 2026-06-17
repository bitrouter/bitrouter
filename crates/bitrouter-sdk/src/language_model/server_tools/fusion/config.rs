//! Parsing of the `bitrouter:fusion` server-tool declaration into a config.
//!
//! A caller declares Fusion by putting a provider-defined tool in the request
//! `tools` array whose name resolves to `fusion` (bare, or namespaced:
//! `bitrouter:fusion`, `bitrouter.fusion`). Its config rides the tool's `args`
//! (tolerating OpenRouter's `parameters` wrapper). The shared
//! [`ServerToolDeclarations`](super::super::declarations::ServerToolDeclarations)
//! hook parses it once, resolving panel/judge models against the outer request
//! model, and stashes it for the toolset to read back.
//!
//! Reference design (behavior modeled after OpenRouter Fusion):
//! <https://openrouter.ai/docs/guides/features/server-tools/fusion>

use serde::{Deserialize, Serialize};

use crate::language_model::types::Tool;

/// Router-tool name the model calls to run a deliberation.
pub const FUSION_TOOL: &str = "fusion";
/// Maximum panel size — matches the documented Fusion bound.
pub const MAX_PANEL: usize = 8;

/// One panel member: a model that answers the prompt in parallel with the rest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelMemberSpec {
    /// The member's model.
    pub model: String,
    /// Provider-native server tools forwarded to this member (e.g. web_search),
    /// in provider-namespaced declaration form; see
    /// [`forwarded_tools`](super::super::declarations::forwarded_tools).
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

/// The judge: compares (does not merge) the panel answers into structured
/// analysis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JudgeSpec {
    /// The judge model.
    pub model: String,
}

/// A fully resolved Fusion invocation (panel/judge models already defaulted to
/// the outer request model where the declaration omitted them).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FusionConfig {
    /// The panel — one entry per model answering in parallel (1..=[`MAX_PANEL`]).
    pub panel: Vec<PanelMemberSpec>,
    /// The judge.
    pub judge: JudgeSpec,
    /// Optional dedicated synthesizer; when `None`, the calling model writes the
    /// final answer from the returned analysis.
    #[serde(default)]
    pub synthesizer: Option<String>,
}

impl FusionConfig {
    /// A degenerate one-member panel judged by the same model.
    pub fn single(model: &str) -> Self {
        FusionConfig {
            panel: vec![PanelMemberSpec {
                model: model.to_string(),
                tools: Vec::new(),
            }],
            judge: JudgeSpec {
                model: model.to_string(),
            },
            synthesizer: None,
        }
    }

    /// Parse a `bitrouter:fusion` declaration. Unspecified panel/judge models
    /// fall back to `parent_model`; the panel is clamped to [`MAX_PANEL`].
    /// Tolerates an OpenRouter-style `parameters` wrapper around the args.
    /// Returns `None` for any tool that is not a Fusion declaration.
    pub fn from_tool(tool: &Tool, parent_model: &str) -> Option<Self> {
        let Tool::ProviderDefined { name, args, .. } = tool else {
            return None;
        };
        if !is_fusion_name(name) {
            return None;
        }
        let args = args.get("parameters").unwrap_or(args);

        let mut panel: Vec<PanelMemberSpec> = args
            .get("panel")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(parse_member).collect())
            .unwrap_or_default();
        if panel.is_empty() {
            panel.push(PanelMemberSpec {
                model: parent_model.to_string(),
                tools: Vec::new(),
            });
        }
        panel.truncate(MAX_PANEL);

        let judge_model = args
            .get("judge")
            .and_then(|j| j.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or(parent_model)
            .to_string();
        let synthesizer = args
            .get("synthesizer")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        Some(FusionConfig {
            panel,
            judge: JudgeSpec { model: judge_model },
            synthesizer,
        })
    }
}

/// Recognise a Fusion declaration by tool name: the bare name, or a namespaced
/// form whose final `:`/`.` segment is the name.
pub fn is_fusion_name(name: &str) -> bool {
    name.rsplit([':', '.']).next().unwrap_or(name) == FUSION_TOOL
}

fn parse_member(v: &serde_json::Value) -> Option<PanelMemberSpec> {
    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let tools = v
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    Some(PanelMemberSpec { model, tools })
}

/// The `server_tools.fusion` config section. Its presence enables the Fusion
/// server tool and the `bitrouter/fusion` model alias; its fields supply the
/// alias defaults (the panel/judge a bare `bitrouter/fusion` request expands
/// to). An explicit `bitrouter:fusion` declaration on a request overrides these.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FusionSettings {
    /// Default panel models the alias expands to.
    pub panel: Vec<String>,
    /// Default judge model (defaults to the first panel model).
    pub judge: Option<String>,
    /// Optional dedicated synthesizer model.
    pub synthesizer: Option<String>,
    /// Alias slug (defaults to `bitrouter/fusion`).
    pub alias: Option<String>,
    /// The model the alias resolves to (defaults to the judge, then the first
    /// panel model).
    pub outer_model: Option<String>,
    /// Provider web tools forwarded to every panel member, in
    /// provider-namespaced declaration form (e.g. `{type: "<provider>:<tool>"}`).
    pub web_tools: Vec<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::ProviderMetadata;

    fn fusion_tool(name: &str, args: serde_json::Value) -> Tool {
        Tool::ProviderDefined {
            id: format!(
                "bitrouter.{}",
                name.rsplit([':', '.']).next().unwrap_or(name)
            ),
            name: name.to_string(),
            args,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    #[test]
    fn parses_panel_judge_and_keeps_order() {
        let tool = fusion_tool(
            "fusion",
            serde_json::json!({
                "panel": [{"model": "anthropic/claude-opus-4.8"},
                          {"model": "openai/gpt-latest"},
                          {"model": "google/gemini-pro"}],
                "judge": {"model": "anthropic/claude-opus-4.8"}
            }),
        );
        let cfg = FusionConfig::from_tool(&tool, "anthropic/claude-opus-4.8").unwrap();
        assert_eq!(
            cfg.panel
                .iter()
                .map(|m| m.model.as_str())
                .collect::<Vec<_>>(),
            vec![
                "anthropic/claude-opus-4.8",
                "openai/gpt-latest",
                "google/gemini-pro"
            ]
        );
        assert_eq!(cfg.judge.model, "anthropic/claude-opus-4.8");
    }

    #[test]
    fn empty_panel_falls_back_to_parent() {
        let cfg = FusionConfig::from_tool(
            &fusion_tool("fusion", serde_json::json!({})),
            "parent/model",
        )
        .unwrap();
        assert_eq!(cfg.panel.len(), 1);
        assert_eq!(cfg.panel[0].model, "parent/model");
        assert_eq!(cfg.judge.model, "parent/model");
    }

    #[test]
    fn clamps_panel_over_eight() {
        let members: Vec<_> = (0..12)
            .map(|i| serde_json::json!({ "model": format!("m/{i}") }))
            .collect();
        let cfg = FusionConfig::from_tool(
            &fusion_tool("fusion", serde_json::json!({ "panel": members })),
            "p/m",
        )
        .unwrap();
        assert_eq!(cfg.panel.len(), MAX_PANEL);
    }

    #[test]
    fn recognises_namespaced_name_and_parameters_wrapper() {
        let tool = fusion_tool(
            "bitrouter:fusion",
            serde_json::json!({
                "parameters": { "judge": {"model": "j/x"}, "synthesizer": "s/y" }
            }),
        );
        let cfg = FusionConfig::from_tool(&tool, "parent/model").unwrap();
        assert_eq!(cfg.judge.model, "j/x");
        assert_eq!(cfg.synthesizer.as_deref(), Some("s/y"));
    }

    #[test]
    fn ignores_non_fusion_tools() {
        assert!(
            FusionConfig::from_tool(&fusion_tool("advisor", serde_json::json!({})), "p").is_none()
        );
        let func = Tool::Function {
            name: "fusion".into(),
            description: None,
            parameters: serde_json::json!({}),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        };
        assert!(FusionConfig::from_tool(&func, "p").is_none());
    }
}
