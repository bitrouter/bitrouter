//! Drive the real processor: it buffers Usage and emits only ResponseCompleted.
use super::*;
use crate::gateway::{
    caller::CallerContext,
    pipeline::{
        stream::{StreamOutcome, StreamProcessor},
        types::PipelineRequest,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role, Usage};

fn candidate(model: &str) -> Key {
    Key {
        namespace: "authorized-credential".into(),
        provider: "fixture".into(),
        model: model.into(),
        account: None,
        endpoint: "https://example.com/v1".into(),
    }
}

#[tokio::test]
async fn responses_terminal_usage_produces_three_samples_and_changes_throughput_order() -> Result<()>
{
    let runtime = Arc::new(Runtime::default());
    let measured = candidate("measured");
    let candidates = [(candidate("unknown"), None), (measured.clone(), None)];
    for sample in 0..3 {
        let mut admission = runtime
            .admit(measured.clone(), &Default::default())
            .map_err(|error| anyhow::anyhow!("admit: {error:?}"))?;
        admission
            .sent()
            .map_err(|error| anyhow::anyhow!("sent: {error:?}"))?;
        // Only the authorization fixture is synthetic. Sampling is driven by
        // the real Profile observer, StreamProcessor, and shared runtime.
        let profile = Profile(Arc::new(Inner {
            execution: Weak::new(),
            runtime: runtime.clone(),
            active: Mutex::new(Some(Active {
                key: measured.clone(),
                admission,
                output_tokens: None,
            })),
        }));
        let prompt = Prompt {
            model: "measured".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let mut context = PipelineContext::new(PipelineRequest::new(
            "measured",
            CallerContext::local(),
            prompt,
        ));
        let mut processor = StreamProcessor::new(
            Vec::new(),
            vec![Arc::new(profile.clone())],
            context.stream_context(),
        );
        let usage = Usage {
            prompt_tokens: 5,
            completion_tokens: 100,
            origin: UsageOrigin::ProviderReported,
            ..Default::default()
        };
        ensure!(
            processor
                .process_part(StreamPart::Usage {
                    usage: usage.clone()
                })
                .await?
                .is_empty()
        );
        let terminal = processor
            .process_part(StreamPart::ResponseCompleted {
                id: format!("response-{sample}"),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: None,
                response_output_commitment: None,
            })
            .await?;
        ensure!(
            matches!(terminal.as_slice(), [StreamPart::ResponseCompleted { usage: Some(observed), .. }] if observed == &usage)
        );
        processor.finish(StreamOutcome::Completed).await;
        context.absorb_stream(processor.into_context());
        ensure!(context.stream_terminal_succeeded());
        let active = profile
            .0
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("profile lock"))?
            .take()
            .context("active admission")?;
        assert_eq!(active.output_tokens, Some(100));
        active.admission.finish(Outcome::Success {
            output_tokens: active.output_tokens,
        });
        let order = runtime
            .order(&candidates, super::super::state::Order::Throughput)
            .map_err(|error| anyhow::anyhow!("order: {error:?}"))?;
        assert_eq!(order, if sample < 2 { vec![0, 1] } else { vec![1, 0] });
    }
    Ok(())
}
