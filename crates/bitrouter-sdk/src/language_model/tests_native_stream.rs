use super::*;
use crate::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission,
};
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
            origin: crate::language_model::types::UsageOrigin::ProviderReported,
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
                id: "reason".into(),
            },
            StreamPart::ReasoningDelta {
                text: "consider".into(),
            },
            StreamPart::ReasoningEnd {
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
    assert_eq!(result.result.content.len(), 4);
    assert!(
        matches!(&result.result.content[0], Content::Reasoning { provider_metadata, .. }
        if provider_metadata.get("anthropic").and_then(|value| value.get("signature"))
            == Some(&serde_json::json!("signed")))
    );
    assert!(
        matches!(&result.result.content[3], Content::ToolCall { arguments, .. }
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
    for terminal_text in ["visible", "different"] {
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
        let pipeline = Arc::new(builder.build()?);
        let result = pipeline
            .execute_native_controlled(request(), control.clone())
            .await;
        if terminal_text == "visible" {
            let result = result?;
            assert!(serde_json::to_string(&result.result.content)?.contains("opaque-native-state"));
            assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, false)]);
        } else {
            assert!(result.is_err());
            assert_eq!(control.reports.lock().await.len(), 1);
            assert_eq!(records.0.lock().await.as_slice(), &[(true, 40, true)]);
        }
    }
    Ok(())
}
