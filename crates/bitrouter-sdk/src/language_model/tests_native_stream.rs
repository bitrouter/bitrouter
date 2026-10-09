use super::*;
use crate::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission,
};
use crate::language_model::stream::{UsagePricing, UsagePricingBracket};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Control {
    reports: Mutex<Vec<NativeAttemptReport>>,
    parts: Mutex<Vec<StreamPart>>,
    cancel: CancellationToken,
    cancel_on_text: bool,
}

#[async_trait]
impl NativeExecutionControl for Control {
    fn observe_stream(&self) -> bool {
        true
    }
    fn provider_response_byte_limit(&self) -> Option<u64> {
        Some(4096)
    }
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        Some(2048)
    }
    async fn on_stream_part(&self, _: &str, part: &StreamPart) {
        if self.cancel_on_text && matches!(part, StreamPart::TextDelta { .. }) {
            self.cancel.cancel();
        }
        self.parts.lock().await.push(part.clone());
    }
    async fn provider_cancelled(&self) {
        self.cancel.cancelled().await;
    }
    async fn plan(&self, plan: NativePlan) -> Result<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: (0..plan.routes.len() as u32).collect(),
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.reports.lock().await.push(report);
    }
}

#[derive(Default, Clone)]
struct Records(Arc<Mutex<Vec<(bool, u64, bool)>>>);
#[async_trait]
impl SettlementRecorder for Records {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        self.0
            .lock()
            .await
            .push((ctx.streamed, ctx.prompt_tokens, ctx.error.is_some()));
        Ok(())
    }
}

fn usage() -> StreamPart {
    StreamPart::Usage {
        usage: Usage {
            prompt_tokens: 40,
            completion_tokens: 7,
            origin: bitrouter_ai::types::UsageOrigin::ProviderReported,
            ..Default::default()
        },
    }
}

fn pipeline(parts: Vec<StreamPart>, records: Records) -> Result<Arc<Pipeline>> {
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routing_table(&["first", "fallback"]))
        .executor(Arc::new(MockExecutor::new(vec![
            MockResponse::Stream(parts),
            MockResponse::Stream(vec![
                StreamPart::TextDelta {
                    text: "wrong fallback".into(),
                },
                StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            ]),
        ])))
        .settlement_recorder(records);
    Ok(Arc::new(builder.build()?))
}

#[tokio::test]
async fn controlled_stream_preserves_blocks_signatures_tools_and_accounting()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let control = Arc::new(Control::default());
    let records = Records::default();
    let result = pipeline(
        vec![
            StreamPart::ReasoningStart {
                source_protocol: None,
                id: "reason".into(),
            },
            StreamPart::ReasoningDelta {
                source_kind: None,
                text: "consider".into(),
            },
            StreamPart::ReasoningEnd {
                native: None,
                id: "reason".into(),
                signature: Some("signed".into()),
            },
            StreamPart::TextStart { id: "one".into() },
            StreamPart::TextDelta {
                text: "first".into(),
            },
            StreamPart::TextEnd { id: "one".into() },
            StreamPart::TextStart { id: "two".into() },
            StreamPart::TextDelta {
                text: "second".into(),
            },
            StreamPart::TextEnd { id: "two".into() },
            StreamPart::ToolCallDelta {
                id: "call".into(),
                name: Some("read".into()),
                arguments: "{\"path\":".into(),
                provider_metadata: Default::default(),
            },
            StreamPart::ToolCallDelta {
                id: "call".into(),
                name: None,
                arguments: "\"src\"}".into(),
                provider_metadata: Default::default(),
            },
            usage(),
            StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            },
        ],
        records.clone(),
    )?
    .execute_native_controlled(request(), control.clone())
    .await?;
    assert_eq!(
        result
            .result
            .generation()
            .ok_or("expected generation result")?
            .content
            .len(),
        4
    );
    assert!(
        matches!(&result.result.generation().ok_or("expected generation result")?.content[0], Content::Reasoning { provider_metadata, .. }
        if provider_metadata.get("anthropic").and_then(|value| value.get("signature"))
            == Some(&serde_json::json!("signed")))
    );
    assert!(
        matches!(&result.result.generation().ok_or("expected generation result")?.content[3], Content::ToolCall { arguments, .. }
        if arguments == "{\"path\":\"src\"}")
    );
    assert_eq!(control.reports.lock().await.len(), 1);
    assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, false)]);
    Ok(())
}

#[tokio::test]
async fn interrupted_stream_retains_observed_usage_without_fallback()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for cancelled in [false, true] {
        let control = Arc::new(Control {
            cancel_on_text: cancelled,
            ..Default::default()
        });
        let records = Records::default();
        let result = pipeline(
            vec![
                usage(),
                StreamPart::TextDelta {
                    text: "partial".into(),
                },
            ],
            records.clone(),
        )?
        .execute_native_controlled(request(), control.clone())
        .await;
        assert!(result.is_err());
        let reports = control.reports.lock().await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].error.is_some());
        assert_eq!(
            reports[0]
                .result
                .as_ref()
                .and_then(|result| result.usage.as_ref())
                .map(|usage| usage.prompt_tokens),
            Some(40)
        );
        assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, true)]);
    }
    Ok(())
}

#[tokio::test]
async fn oversized_stream_frame_and_duplicate_tool_identity_are_rejected()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let complete = StreamPart::ToolCallDelta {
        id: "duplicate".into(),
        name: Some("read".into()),
        arguments: "{}".into(),
        provider_metadata: Default::default(),
    };
    for parts in [
        vec![
            usage(),
            StreamPart::TextDelta {
                text: "x".repeat(8192),
            },
        ],
        vec![usage(), complete.clone(), complete],
    ] {
        let control = Arc::new(Control::default());
        let records = Records::default();
        assert!(
            pipeline(parts, records.clone())?
                .execute_native_controlled(request(), control.clone())
                .await
                .is_err()
        );
        assert_eq!(control.reports.lock().await.len(), 1);
        assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, true)]);
    }
    Ok(())
}

#[tokio::test]
async fn controlled_http_stream_retains_complete_responses_private_output()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    for (terminal_text, rewrite) in [("visible", false), ("different", false), ("visible", true)] {
        let server = MockServer::start().await;
        let terminal = serde_json::json!({
            "type":"response.completed", "response": {
                "id":"resp-native", "status":"completed", "store":false,
                "output":[
                    {"id":"reasoning-native","type":"reasoning","summary":[],"encrypted_content":"opaque-native-state"},
                    {"id":"message-native","type":"message","role":"assistant","content":[{"type":"output_text","text":terminal_text,"annotations":[]}]}
                ],
                "usage":{"input_tokens":40,"output_tokens":7}
            }
        });
        let body = format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-native\",\"status\":\"in_progress\"}}}}\n\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"visible\"}}\n\ndata: {terminal}\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut route = target("responses-fixture");
        route.api_base = server.uri();
        route.api_protocol = ApiProtocol::Responses;
        let table = StaticRoutingTable::new();
        table.insert("test-model", vec![route]);
        let records = Records::default();
        let control = Arc::new(Control::default());
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(table))
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .settlement_recorder(records.clone());
        if rewrite {
            builder.stream_hook(ScriptedStreamHook {
                interest: StreamInterest::all(),
                mode: StreamMode::UppercaseText,
                ended_with: Default::default(),
            });
        }
        let pipeline = Arc::new(builder.build()?);
        let result = pipeline
            .execute_native_controlled(request(), control.clone())
            .await;
        if terminal_text == "visible" && !rewrite {
            let result = result?;
            assert!(
                serde_json::to_string(
                    &result
                        .result
                        .generation()
                        .ok_or("expected generation result")?
                        .content
                )?
                .contains("opaque-native-state")
            );
            assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, false)]);
        } else {
            assert!(result.is_err());
            assert_eq!(control.reports.lock().await.len(), 1);
            assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, true)]);
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_reasoning_preserves_interleaved_lanes_and_rejects_changed_text()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for retained in ["AC", "rewritten"] {
        let native = NativeReasoning::Responses(serde_json::json!({
            "type":"reasoning", "id":"r", "summary":[{"type":"summary_text", "text":retained}],
            "content":[{"type":"reasoning_text", "text":"B"}]
        }));
        let result = pipeline(
            vec![
                StreamPart::ReasoningStart {
                    id: "r".into(),
                    source_protocol: Some(ApiProtocol::Responses),
                },
                StreamPart::ReasoningDelta {
                    text: "A".into(),
                    source_kind: Some(ReasoningTextKind::Summary),
                },
                StreamPart::ReasoningDelta {
                    text: "B".into(),
                    source_kind: Some(ReasoningTextKind::Text),
                },
                StreamPart::ReasoningDelta {
                    text: "C".into(),
                    source_kind: Some(ReasoningTextKind::Summary),
                },
                StreamPart::ReasoningEnd {
                    id: "r".into(),
                    signature: None,
                    native: Some(native.clone()),
                },
                usage(),
                StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            ],
            Records::default(),
        )?
        .execute_native_controlled(request(), Arc::new(Control::default()))
        .await;
        if retained == "AC" {
            let result = result?;
            assert!(
                matches!(&result.result.generation().ok_or("generation")?.content[0],
                Content::Reasoning { text, native: Some(saved), .. } if text == "ACB" && saved == &native)
            );
        } else {
            assert!(result.is_err());
        }
    }
    Ok(())
}

struct LivePricing;
#[async_trait]
impl RoutingTable for LivePricing {
    async fn route_chain(
        &self,
        _: &str,
        _: &RoutingPrefs,
        _: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        Ok(vec![target("priced")])
    }
    fn usage_pricing(&self, _: &str, _: &RoutingTarget) -> Option<UsagePricing> {
        Some(UsagePricing {
            base: UsagePricingBracket {
                input_micro_usd_per_token: Some(100.0),
                output_micro_usd_per_token: Some(1.0),
                ..Default::default()
            },
            ..Default::default()
        })
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    fn model_info(&self, _: &str) -> Option<ModelInfo> {
        None
    }
    async fn reload(&self) -> Result<()> {
        Ok(())
    }
}

struct FreezePricing(Option<UsagePricing>);
#[async_trait]
impl RouteHook for FreezePricing {
    async fn resolve(&self, _: &mut Vec<RoutingTarget>, _: &mut PipelineContext) -> Result<()> {
        Ok(())
    }
    async fn after_resolve(
        &self,
        chain: &[RoutingTarget],
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        for target in chain {
            ctx.emit(crate::language_model::stream::UsagePricingSnapshot {
                target: crate::language_model::stream::PricingTargetKey::from_target(target),
                pricing: self.0.clone(),
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn native_stream_uses_frozen_rates_including_unknown()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for frozen in [
        None,
        Some(UsagePricing {
            base: UsagePricingBracket {
                input_micro_usd_per_token: Some(1.0),
                output_micro_usd_per_token: Some(100.0),
                ..Default::default()
            },
            ..Default::default()
        }),
    ] {
        let records = Records::default();
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(LivePricing))
            .route_hook(FreezePricing(frozen))
            .settlement_recorder(records.clone())
            .executor(Arc::new(MockExecutor::new(vec![MockResponse::Stream(
                vec![
                    StreamPart::Usage {
                        usage: Usage {
                            prompt_tokens: 100,
                            completion_tokens: 1,
                            origin: UsageOrigin::ProviderReported,
                            ..Default::default()
                        },
                    },
                    StreamPart::Usage {
                        usage: Usage {
                            prompt_tokens: 1,
                            completion_tokens: 100,
                            origin: UsageOrigin::ProviderReported,
                            ..Default::default()
                        },
                    },
                    StreamPart::Finish {
                        reason: FinishReason::Stop,
                    },
                ],
            )])));
        Arc::new(builder.build()?)
            .execute_native_controlled(request(), Arc::new(Control::default()))
            .await?;
        assert_eq!(records.0.lock().await.as_slice(), &[(true, 1, false)]);
    }
    Ok(())
}

struct DropReasoning;
#[async_trait]
impl StreamHook for DropReasoning {
    async fn on_stream_end(&self, _: &mut StreamContext, _: &StreamOutcome) -> Result<()> {
        Ok(())
    }
    fn interest(&self) -> StreamInterest {
        StreamInterest::all()
    }
    async fn on_part(&self, _: &mut StreamContext, part: StreamPart) -> Result<StreamAction> {
        Ok(
            if matches!(
                part,
                StreamPart::ReasoningStart { .. }
                    | StreamPart::ReasoningDelta { .. }
                    | StreamPart::ReasoningEnd { .. }
            ) {
                StreamAction::Replace(Vec::new())
            } else {
                StreamAction::Pass
            },
        )
    }
}

#[tokio::test]
async fn native_terminal_cannot_restore_reasoning_removed_by_stream_policy()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    let item = serde_json::json!({"id":"r", "type":"reasoning", "summary":[{"type":"summary_text","text":"private"}], "encrypted_content":"opaque"});
    let body = [
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"r","summary":[]}}),
        serde_json::json!({"type":"response.reasoning_summary_text.delta","item_id":"r","delta":"private"}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"id":"r","type":"reasoning"}}),
        serde_json::json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[item],"usage":{"input_tokens":40,"output_tokens":7}}}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut route = target("responses");
    route.api_protocol = ApiProtocol::Responses;
    route.api_base = server.uri();
    let table = StaticRoutingTable::new();
    table.insert("test-model", vec![route]);
    let records = Records::default();
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(table))
        .executor(Arc::new(HttpExecutor::with_defaults()?))
        .stream_hook(DropReasoning)
        .settlement_recorder(records.clone());
    assert!(
        Arc::new(builder.build()?)
            .execute_native_controlled(request(), Arc::new(Control::default()))
            .await
            .is_err()
    );
    assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, true)]);
    Ok(())
}
