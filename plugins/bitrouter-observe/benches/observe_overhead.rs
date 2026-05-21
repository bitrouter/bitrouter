//! Performance benchmarks for observability overhead.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashMap;

use bitrouter_observe::otel::{OtelConfig, OtelExporter};
use bitrouter_sdk::{
    caller::CallerContext,
    language_model::{
        Content, ExecutionResult, FinishReason, GenerateResult, ObserveHook, Phase,
        PipelineContext, PipelineRequest, Prompt, RequestOutcome, Usage,
    },
};

/// Create a mock pipeline context for testing.
fn mock_context(id: usize) -> PipelineContext {
    PipelineContext::new(PipelineRequest {
        request_id: format!("req_{}", id),
        model: "claude-3-sonnet".to_string(),
        caller: CallerContext::new(format!("key_{}", id % 100), format!("user_{}", id % 10)),
        headers: http::HeaderMap::new(),
        prompt: Prompt {
            system: Some("You are a helpful assistant".to_string()),
            messages: vec![],
            params: Default::default(),
        },
    })
}

/// Create a mock execution result.
fn mock_execution_result() -> ExecutionResult {
    ExecutionResult {
        provider_id: "anthropic".to_string(),
        model_id: "claude-3-sonnet-20240229".to_string(),
        account_label: Some("primary".to_string()),
        result: GenerateResult {
            content: vec![Content::Text {
                text: "Hello, world!".to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                reasoning_tokens: 0,
            }),
            finish_reason: Some(FinishReason::Stop),
        },
        latency_ms: 250,
        generation_time_ms: 200,
    }
}

/// Process a mock request through the observe hook.
async fn process_mock_request(hook: Option<&OtelExporter>, ctx: &mut PipelineContext) {
    if let Some(h) = hook {
        // Simulate phases using the actual ObserveHook trait methods
        h.after_phase(Phase::PreRequest, ctx).await;
        h.after_phase(Phase::Route, ctx).await;
        
        // Add execution result
        ctx.execution_result = Some(mock_execution_result());
        h.after_phase(Phase::Execution, ctx).await;
        h.after_phase(Phase::Settlement, ctx).await;
        
        // End request
        h.on_request_end(ctx, &RequestOutcome::Completed).await;
    } else {
        // Baseline: just set the execution result
        ctx.execution_result = Some(mock_execution_result());
    }
}

fn bench_request_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("observe_overhead");
    
    // Create runtime for async benchmarks
    let runtime = tokio::runtime::Runtime::new().unwrap();
    
    // Baseline: No observability
    group.bench_function("baseline_no_observe", |b| {
        b.to_async(&runtime).iter(|| async {
            let mut ctx = mock_context(0);
            process_mock_request(None, &mut ctx).await;
            black_box(ctx);
        });
    });
    
    // With OTel (no-op mode - just span creation, no export)
    let noop_exporter = OtelExporter::new_noop();
    group.bench_function("otel_spans_only", |b| {
        b.to_async(&runtime).iter(|| async {
            let mut ctx = mock_context(0);
            process_mock_request(Some(&noop_exporter), &mut ctx).await;
            black_box(ctx);
        });
    });
    
    // TODO: Add benchmark with real export to local collector when OTel setup is complete
    
    group.finish();
}

fn bench_cardinality_capping(c: &mut Criterion) {
    use bitrouter_observe::otel::CardinalityLimiter;
    
    let mut group = c.benchmark_group("cardinality");
    
    for size in [100, 1000, 10000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let limiter = CardinalityLimiter::new(1024);
            let keys: Vec<String> = (0..size).map(|i| format!("key_{}", i)).collect();
            
            b.iter(|| {
                for key in &keys {
                    black_box(limiter.cap(key));
                }
            });
        });
    }
    
    // Benchmark the "other" case (all keys over limit)
    group.bench_function("cardinality_all_other", |b| {
        let limiter = CardinalityLimiter::new(10);
        // Pre-fill to capacity
        for i in 0..10 {
            limiter.cap(&format!("key_{}", i));
        }
        
        // Now all new keys will be "other"
        let keys: Vec<String> = (100..200).map(|i| format!("key_{}", i)).collect();
        
        b.iter(|| {
            for key in &keys {
                black_box(limiter.cap(key));
            }
        });
    });
    
    group.finish();
}

fn bench_concurrent_requests(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    
    for concurrency in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            concurrency,
            |b, &concurrency| {
                let exporter = OtelExporter::new_noop();
                
                b.to_async(&runtime).iter(|| async {
                    let tasks: Vec<_> = (0..concurrency)
                        .map(|i| {
                            let exp = &exporter;
                            async move {
                                let mut ctx = mock_context(i);
                                process_mock_request(Some(exp), &mut ctx).await;
                            }
                        })
                        .collect();
                    
                    futures::future::join_all(tasks).await;
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_request_lifecycle,
    bench_cardinality_capping,
    bench_concurrent_requests
);
criterion_main!(benches);