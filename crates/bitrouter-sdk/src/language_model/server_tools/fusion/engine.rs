//! The Fusion engine: fan the prompt out to the panel in parallel, judge the
//! answers, and return structured analysis plus the aggregated token usage. The
//! calling model writes the final answer from the analysis (or an optional
//! synthesizer model does).
//!
//! Reference design: <https://openrouter.ai/docs/guides/features/server-tools/fusion>

use std::sync::Arc;

use futures::future::join_all;
use serde_json::json;
use tracing::Instrument;

use super::config::{FusionConfig, forwarded_tools};
use super::judge::{JudgeAnalysis, analysis_schema, judge_system_prompt};
use crate::language_model::server_tools::nested::{NestedRequest, NestedRunner};
use crate::language_model::server_tools::toolset::ToolContext;
use crate::language_model::types::{ResponseFormat, ToolResultOutput, Usage};

/// The engine's result: the tool output (the analysis JSON, or synthesized
/// prose) plus the usage summed across panel + judge (+ synthesizer), so the
/// caller can surface it as server-tool usage.
pub struct FusionOutcome {
    /// What the calling model receives as the tool result.
    pub output: ToolResultOutput,
    /// Token usage aggregated across every nested completion.
    pub usage: Usage,
}

/// Run one Fusion deliberation.
#[tracing::instrument(
    skip_all,
    fields(panel_size = config.panel.len(), judge_model = %config.judge.model)
)]
pub async fn run_fusion(
    config: &FusionConfig,
    runner: Arc<dyn NestedRunner>,
    prompt: &str,
    ctx: &ToolContext,
) -> Result<FusionOutcome, String> {
    let mut usage = Usage::default();

    // 1) Panel — every member answers the same prompt in parallel.
    let panel_futs = config.panel.iter().map(|member| {
        let runner = runner.clone();
        let req = NestedRequest {
            model: member.model.clone(),
            system: None,
            user: prompt.to_string(),
            tools: forwarded_tools(&member.tools),
            response_format: None,
        };
        let span = tracing::info_span!("fusion.panel", model = %member.model);
        async move { runner.run(req, ctx).await }.instrument(span)
    });
    let answers: Vec<(String, String)> = join_all(panel_futs)
        .await
        .into_iter()
        .filter_map(Result::ok)
        .map(|o| {
            accumulate(&mut usage, &o.usage);
            (o.model, o.text)
        })
        .collect();
    if answers.is_empty() {
        return Err("every panel member failed".to_string());
    }

    // 2) Judge — compare (not merge) the answers into structured analysis.
    // The judge requests structured output; pick a judge model that advertises
    // it, since a provider that rejects `response_format` fails the call before
    // the lenient parser (which only salvages JSON-in-text from models that
    // accept the request) can run.
    let judge_req = NestedRequest {
        model: config.judge.model.clone(),
        system: Some(judge_system_prompt().to_string()),
        user: render_judge_input(prompt, &answers),
        tools: Vec::new(),
        response_format: Some(ResponseFormat::JsonSchema {
            name: Some("fusion_analysis".to_string()),
            description: Some("Comparison of the panel answers.".to_string()),
            strict: Some(true),
            schema: analysis_schema(),
        }),
    };
    let judge_out = async { runner.run(judge_req, ctx).await }
        .instrument(tracing::info_span!("fusion.judge", model = %config.judge.model))
        .await?;
    accumulate(&mut usage, &judge_out.usage);
    let analysis = JudgeAnalysis::parse_lenient(&judge_out.text)?;
    let analysis_json = serde_json::to_value(&analysis).map_err(|e| e.to_string())?;

    // 3) Result — hand the analysis to the calling model, or synthesize prose.
    let panel_models: Vec<String> = answers.iter().map(|(m, _)| m.clone()).collect();
    let output = match &config.synthesizer {
        None => ToolResultOutput::Json {
            value: json!({ "panel_models": panel_models, "analysis": analysis_json }),
        },
        Some(synth_model) => {
            let synth_req = NestedRequest {
                model: synth_model.clone(),
                system: Some(
                    "Write the best possible final answer to the user's prompt, \
                     grounded in the provided comparison of expert answers."
                        .to_string(),
                ),
                user: format!(
                    "Prompt:\n{prompt}\n\nComparison (JSON):\n{}",
                    serde_json::to_string_pretty(&analysis_json).unwrap_or_default()
                ),
                tools: Vec::new(),
                response_format: None,
            };
            match async { runner.run(synth_req, ctx).await }
                .instrument(tracing::info_span!("fusion.synthesizer", model = %synth_model))
                .await
            {
                Ok(synth) => {
                    accumulate(&mut usage, &synth.usage);
                    ToolResultOutput::Text { value: synth.text }
                }
                // Degrade gracefully: the panel + judge work is already done, so
                // hand the calling model the analysis to write the answer from
                // rather than discarding everything on a synthesizer failure.
                Err(e) => {
                    tracing::warn!(error = %e, "fusion synthesizer failed; returning analysis");
                    ToolResultOutput::Json {
                        value: json!({ "panel_models": panel_models, "analysis": analysis_json }),
                    }
                }
            }
        }
    };
    Ok(FusionOutcome { output, usage })
}

fn render_judge_input(prompt: &str, answers: &[(String, String)]) -> String {
    let mut s = format!("Original prompt:\n{prompt}\n\nAnswers to compare:\n");
    for (i, (model, text)) in answers.iter().enumerate() {
        s.push_str(&format!(
            "\n--- Answer {} (model: {}) ---\n{}\n",
            i + 1,
            model,
            text
        ));
    }
    s
}

/// Sum one nested call's usage into the running total.
fn accumulate(total: &mut Usage, add: &Usage) {
    total.prompt_tokens += add.prompt_tokens;
    total.completion_tokens += add.completion_tokens;
    total.reasoning_tokens += add.reasoning_tokens;
    total.cache_read_tokens += add.cache_read_tokens;
    total.cache_write_tokens += add.cache_write_tokens;
    total.web_search_count += add.web_search_count;
}

#[cfg(test)]
mod tests {
    use super::super::config::{FusionConfig, JudgeSpec, PanelMemberSpec};
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::nested::{NestedOutcome, NestedRequest, NestedRunner};
    use crate::language_model::server_tools::toolset::ToolContext;
    use crate::language_model::types::{ToolResultOutput, Usage};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Records every nested request; replays a fixed analysis for the judge call
    /// (the one carrying a response_format) and a per-model answer otherwise.
    struct MockRunner {
        seen: Mutex<Vec<NestedRequest>>,
    }
    #[async_trait]
    impl NestedRunner for MockRunner {
        async fn run(
            &self,
            req: NestedRequest,
            _ctx: &ToolContext,
        ) -> Result<NestedOutcome, String> {
            let is_judge = req.response_format.is_some();
            let model = req.model.clone();
            self.seen.lock().unwrap().push(req);
            let text = if is_judge {
                "{\"consensus\":[\"sky is blue\"],\"contradictions\":[],\"partial_coverage\":[],\
                 \"unique_insights\":[],\"blind_spots\":[]}"
                    .to_string()
            } else {
                format!("answer from {model}")
            };
            Ok(NestedOutcome {
                model,
                text,
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    ..Default::default()
                },
            })
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(CallerContext::local(), HashMap::new())
    }

    fn cfg(panel: &[&str], judge: &str, synth: Option<&str>) -> FusionConfig {
        FusionConfig {
            panel: panel
                .iter()
                .map(|m| PanelMemberSpec {
                    model: m.to_string(),
                    tools: Vec::new(),
                })
                .collect(),
            judge: JudgeSpec {
                model: judge.to_string(),
            },
            synthesizer: synth.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn fans_out_panel_then_judges_and_returns_analysis() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let out = run_fusion(
            &cfg(&["a/1", "b/2"], "judge/x", None),
            runner.clone(),
            "what color is the sky?",
            &ctx(),
        )
        .await
        .unwrap();

        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "2 panel + 1 judge");
        assert_eq!(
            seen.iter().filter(|r| r.response_format.is_some()).count(),
            1,
            "only the judge carries a response_format"
        );
        let value = match out.output {
            ToolResultOutput::Json { value } => value,
            _ => panic!("expected a JSON analysis output"),
        };
        assert_eq!(value["analysis"]["consensus"][0], "sky is blue");
        assert_eq!(value["panel_models"].as_array().unwrap().len(), 2);
        // usage summed across 3 calls (each 1 prompt + 2 completion).
        assert_eq!(out.usage.prompt_tokens, 3);
        assert_eq!(out.usage.completion_tokens, 6);
    }

    #[tokio::test]
    async fn synthesizer_produces_text_and_adds_a_call() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let out = run_fusion(
            &cfg(&["a/1"], "judge/x", Some("synth/z")),
            runner.clone(),
            "q",
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(
            runner.seen.lock().unwrap().len(),
            3,
            "1 panel + 1 judge + 1 synthesizer"
        );
        assert!(matches!(out.output, ToolResultOutput::Text { .. }));
    }

    #[tokio::test]
    async fn synthesizer_failure_falls_back_to_analysis() {
        // Panel + judge succeed; the synthesizer fails → the engine returns the
        // analysis JSON instead of erroring out.
        struct SynthFails;
        #[async_trait]
        impl NestedRunner for SynthFails {
            async fn run(
                &self,
                req: NestedRequest,
                _ctx: &ToolContext,
            ) -> Result<NestedOutcome, String> {
                if req.response_format.is_some() {
                    return Ok(NestedOutcome {
                        model: req.model,
                        text: "{\"consensus\":[\"ok\"]}".to_string(),
                        usage: Usage::default(),
                    });
                }
                if req.model == "synth/z" {
                    return Err("synth down".to_string());
                }
                Ok(NestedOutcome {
                    model: req.model,
                    text: "answer".to_string(),
                    usage: Usage::default(),
                })
            }
        }
        let out = run_fusion(
            &cfg(&["a/1"], "j", Some("synth/z")),
            Arc::new(SynthFails),
            "q",
            &ctx(),
        )
        .await
        .unwrap();
        let value = match out.output {
            ToolResultOutput::Json { value } => value,
            _ => panic!("expected analysis fallback"),
        };
        assert_eq!(value["analysis"]["consensus"][0], "ok");
    }

    #[tokio::test]
    async fn all_panel_failures_is_an_error() {
        struct FailRunner;
        #[async_trait]
        impl NestedRunner for FailRunner {
            async fn run(
                &self,
                _req: NestedRequest,
                _ctx: &ToolContext,
            ) -> Result<NestedOutcome, String> {
                Err("boom".to_string())
            }
        }
        let res = run_fusion(&cfg(&["a/1"], "j", None), Arc::new(FailRunner), "q", &ctx()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn surviving_panel_members_still_judge() {
        // One member fails, one succeeds → judge still runs over the survivor.
        struct PartialRunner;
        #[async_trait]
        impl NestedRunner for PartialRunner {
            async fn run(
                &self,
                req: NestedRequest,
                _ctx: &ToolContext,
            ) -> Result<NestedOutcome, String> {
                if req.response_format.is_some() {
                    return Ok(NestedOutcome {
                        model: req.model,
                        text: "{\"consensus\":[\"ok\"]}".to_string(),
                        usage: Usage::default(),
                    });
                }
                if req.model == "bad/1" {
                    return Err("down".to_string());
                }
                Ok(NestedOutcome {
                    model: req.model,
                    text: "good answer".to_string(),
                    usage: Usage::default(),
                })
            }
        }
        let out = run_fusion(
            &cfg(&["bad/1", "good/2"], "j", None),
            Arc::new(PartialRunner),
            "q",
            &ctx(),
        )
        .await
        .unwrap();
        let value = match out.output {
            ToolResultOutput::Json { value } => value,
            _ => panic!("expected json"),
        };
        // Only the surviving member is listed.
        assert_eq!(value["panel_models"], serde_json::json!(["good/2"]));
    }
}
