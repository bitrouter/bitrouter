use futures::stream;
use serde_json::json;

use super::collect_generate;
use crate::error::{ModelError, Result};
use crate::types::{
    ApiProtocol, Content, FinishReason, ProviderMetadata, StreamPart, ToolResultOutput, Usage,
};

fn parts(parts: Vec<StreamPart>) -> impl futures::Stream<Item = Result<StreamPart>> + Unpin {
    stream::iter(parts.into_iter().map(Ok))
}

#[tokio::test]
async fn keeps_block_boundaries_and_interleaved_tool_identity() -> Result<()> {
    let metadata =
        ProviderMetadata::from([("openai".into(), json!({"itemId":"item_a","type":"custom"}))]);
    let result = collect_generate(parts(vec![
        StreamPart::TextStart { id: "first".into() },
        StreamPart::TextDelta {
            text: "before ".into(),
        },
        StreamPart::ToolCallDelta {
            id: "call_a".into(),
            name: None,
            arguments: "raw ".into(),
            provider_metadata: Default::default(),
        },
        StreamPart::ToolCallDelta {
            id: "call_b".into(),
            name: Some("other".into()),
            arguments: "{}".into(),
            provider_metadata: Default::default(),
        },
        StreamPart::TextDelta {
            text: "after".into(),
        },
        StreamPart::ToolCallDelta {
            id: "call_a".into(),
            name: Some("custom".into()),
            arguments: "input".into(),
            provider_metadata: metadata.clone(),
        },
        StreamPart::TextEnd { id: "first".into() },
        StreamPart::TextStart { id: "empty".into() },
        StreamPart::TextEnd { id: "empty".into() },
        StreamPart::TextStart { id: "last".into() },
        StreamPart::TextDelta {
            text: "last".into(),
        },
        StreamPart::TextEnd { id: "last".into() },
        StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        },
    ]))
    .await?;
    assert_eq!(result.content.len(), 5);
    assert!(matches!(&result.content[0], Content::Text { text, .. } if text == "before after"));
    assert!(
        matches!(&result.content[1], Content::ToolCall { id, name, arguments, provider_metadata, .. } if id == "call_a" && name == "custom" && arguments == "raw input" && *provider_metadata == metadata)
    );
    assert!(matches!(&result.content[2], Content::ToolCall { id, .. } if id == "call_b"));
    assert!(matches!(&result.content[3], Content::Text { text, .. } if text.is_empty()));
    assert!(matches!(&result.content[4], Content::Text { text, .. } if text == "last"));
    Ok(())
}

#[tokio::test]
async fn retains_signed_reasoning_and_server_tool_pair_without_executing_it() -> Result<()> {
    let result = collect_generate(parts(vec![
        StreamPart::ReasoningStart {
            id: "thinking".into(),
            source_protocol: None,
        },
        StreamPart::ReasoningDelta {
            text: "reasoning".into(),
            source_kind: None,
        },
        StreamPart::ReasoningEnd {
            id: "thinking".into(),
            signature: Some("continuity-signature".into()),
            native: None,
        },
        StreamPart::ServerToolCall {
            id: "mcp_call".into(),
            name: "read".into(),
            arguments: "{}".into(),
            server_name: Some("fixture-server".into()),
            dynamic: true,
        },
        StreamPart::ServerToolResult {
            call_id: "mcp_call".into(),
            tool_name: Some("read".into()),
            output: ToolResultOutput::Text {
                value: "read result".into(),
            },
            dynamic: true,
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ]))
    .await?;
    assert!(
        matches!(&result.content[0], Content::Reasoning { text, provider_metadata, .. } if text == "reasoning" && provider_metadata["anthropic"]["signature"] == "continuity-signature")
    );
    assert!(
        matches!(&result.content[1], Content::ToolCall { provider_executed: true, dynamic: true, provider_metadata, .. } if provider_metadata["anthropic"]["serverName"] == "fixture-server")
    );
    assert!(
        matches!(&result.content[2], Content::ToolResult { call_id, dynamic: true, .. } if call_id == "mcp_call")
    );
    Ok(())
}

#[tokio::test]
async fn final_usage_is_a_snapshot_and_incomplete_is_not_stop() -> Result<()> {
    let final_usage = Usage {
        prompt_tokens: 12,
        completion_tokens: 4,
        cache_read_tokens: 5,
        reasoning_tokens: 1,
        ..Default::default()
    };
    let result = collect_generate(parts(vec![
        StreamPart::ResponseStarted {
            id: "response".into(),
            source_protocol: ApiProtocol::Responses,
        },
        StreamPart::Usage {
            usage: Usage {
                prompt_tokens: 12,
                ..Default::default()
            },
        },
        StreamPart::ResponseCompleted {
            id: "response".into(),
            source_protocol: ApiProtocol::Responses,
            status: "incomplete".into(),
            usage: Some(final_usage.clone()),
            response_output_commitment: None,
        },
    ]))
    .await?;
    assert_eq!(result.usage, Some(final_usage.clone()));
    assert_eq!(result.finish_reason, Some(FinishReason::Length));
    assert_eq!(result.response_id.as_deref(), Some("response"));
    let trailing = collect_generate(parts(vec![
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
        StreamPart::Usage {
            usage: final_usage.clone(),
        },
    ]))
    .await?;
    assert_eq!(trailing.usage, Some(final_usage));
    Ok(())
}

#[tokio::test]
async fn never_returns_success_after_a_late_stream_error() {
    let result = collect_generate(stream::iter(vec![
        Ok(StreamPart::TextDelta {
            text: "partial".into(),
        }),
        Ok(StreamPart::Finish {
            reason: FinishReason::Stop,
        }),
        Err(ModelError::Provider {
            status: 429,
            message: "late failure".into(),
        }),
    ]))
    .await;
    assert!(matches!(
        result,
        Err(ModelError::Provider { status: 429, .. })
    ));
}

#[tokio::test]
async fn rejects_missing_terminal_and_contradictory_lifecycle() {
    let invalid_streams = vec![
        vec![StreamPart::TextDelta {
            text: "partial".into(),
        }],
        vec![
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
            StreamPart::TextDelta {
                text: "late".into(),
            },
        ],
        vec![
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
            StreamPart::Finish {
                reason: FinishReason::Length,
            },
        ],
        vec![StreamPart::ResponseCompleted {
            id: "response".into(),
            source_protocol: ApiProtocol::Responses,
            status: "failed".into(),
            usage: None,
            response_output_commitment: None,
        }],
        vec![
            StreamPart::ResponseStarted {
                id: "first".into(),
                source_protocol: ApiProtocol::Responses,
            },
            StreamPart::ResponseCompleted {
                id: "second".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: None,
                response_output_commitment: None,
            },
        ],
        vec![
            StreamPart::TextStart { id: "first".into() },
            StreamPart::TextEnd {
                id: "second".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ],
    ];
    for input in invalid_streams {
        assert!(matches!(
            collect_generate(parts(input)).await,
            Err(ModelError::InvalidResponse { .. })
        ));
    }
}

#[tokio::test]
async fn rejects_changed_tool_identity_instead_of_discarding_it() {
    for metadata_conflict in [false, true] {
        let first = ProviderMetadata::from([("openai".into(), json!({"itemId":"first"}))]);
        let second = if metadata_conflict {
            ProviderMetadata::from([("openai".into(), json!({"itemId":"second"}))])
        } else {
            first.clone()
        };
        let input = vec![
            StreamPart::ToolCallDelta {
                id: "call".into(),
                name: Some("first".into()),
                arguments: "".into(),
                provider_metadata: first,
            },
            StreamPart::ToolCallDelta {
                id: "call".into(),
                name: Some(if metadata_conflict { "first" } else { "second" }.into()),
                arguments: "".into(),
                provider_metadata: second,
            },
            StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            },
        ];
        assert!(matches!(
            collect_generate(parts(input)).await,
            Err(ModelError::InvalidResponse { .. })
        ));
    }
}
