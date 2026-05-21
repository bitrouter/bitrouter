//! Performance benchmarks for the cardinality limiter — the only piece of
//! the OTel exporter that can be exercised without spinning up the SDK
//! against a live collector. End-to-end overhead is better measured against
//! a real Jaeger / OTel-collector via `scripts/test_observability.sh`.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use bitrouter_observe::otel::CardinalityLimiter;

fn bench_cardinality_capping(c: &mut Criterion) {
    let mut group = c.benchmark_group("cardinality");

    for size in [100, 1000, 10000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let limiter = CardinalityLimiter::new(1024);
            let keys: Vec<String> = (0..size).map(|i| format!("key_{i}")).collect();
            b.iter(|| {
                for key in &keys {
                    black_box(limiter.cap(key));
                }
            });
        });
    }

    group.bench_function("cardinality_all_other", |b| {
        let limiter = CardinalityLimiter::new(10);
        for i in 0..10 {
            limiter.cap(&format!("key_{i}"));
        }
        let keys: Vec<String> = (100..200).map(|i| format!("key_{i}")).collect();
        b.iter(|| {
            for key in &keys {
                black_box(limiter.cap(key));
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_cardinality_capping);
criterion_main!(benches);
