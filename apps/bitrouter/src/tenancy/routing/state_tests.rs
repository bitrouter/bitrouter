use super::*;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn key(namespace: &str, model: &str) -> Key {
    Key {
        namespace: namespace.into(),
        provider: "provider".into(),
        model: model.into(),
        account: None,
        endpoint: "https://provider.example/v1".into(),
    }
}
fn admitted(
    runtime: &Arc<Runtime>,
    key: Key,
    limits: &Limits,
    now: Instant,
) -> Result<Admission, Box<dyn std::error::Error>> {
    runtime
        .admit_at(key, limits, now)
        .map_err(|error| format!("admission refused: {error:?}").into())
}
fn send(permit: &mut Admission, now: Instant) -> TestResult {
    permit
        .sent_at(now)
        .map_err(|error| format!("send refused: {error:?}").into())
}
fn sample(
    runtime: &Arc<Runtime>,
    key: &Key,
    now: Instant,
    elapsed: Duration,
    tokens: Option<u64>,
) -> TestResult {
    let mut permit = admitted(runtime, key.clone(), &Limits::default(), now)?;
    send(&mut permit, now)?;
    permit.finish_at(
        Outcome::Success {
            output_tokens: tokens,
        },
        now + elapsed,
    );
    Ok(())
}
fn trip(runtime: &Arc<Runtime>, key: &Key, now: Instant) -> TestResult {
    for _ in 0..FAILURES {
        let mut permit = admitted(runtime, key.clone(), &Limits::default(), now)?;
        send(&mut permit, now)?;
        permit.finish_at(Outcome::Unavailable, now);
    }
    Ok(())
}
#[test]
fn rpm_concurrency_is_atomic_across_both_limits() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let barrier = Arc::new(Barrier::new(32));
    let count = Arc::new(AtomicUsize::new(0));
    let now = runtime.epoch;
    std::thread::scope(|scope| {
        for _ in 0..32 {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let count = count.clone();
            scope.spawn(move || {
                barrier.wait();
                if let Ok(mut permit) = runtime.admit_at(
                    key("one", "a"),
                    &Limits {
                        provider: Some(("*".into(), 7)),
                        model: Some(4),
                    },
                    now,
                ) && permit.sent_at(now).is_ok()
                {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(count.load(Ordering::SeqCst), 4);
    let limits = Limits {
        provider: Some(("*".into(), 7)),
        model: Some(4),
    };
    for _ in 0..3 {
        let mut permit = admitted(&runtime, key("one", "b"), &limits, now)?;
        send(&mut permit, now)?;
    }
    assert!(matches!(
        runtime.admit_at(key("one", "b"), &limits, now),
        Err(Refusal::RateLimited)
    ));
    Ok(())
}
#[test]
fn unsent_cancel_refunds_sent_cancel_retains_rpm() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let limits = Limits {
        provider: Some(("*".into(), 1)),
        model: Some(1),
    };
    drop(admitted(&runtime, key("one", "a"), &limits, now)?);
    let mut permit = admitted(&runtime, key("one", "a"), &limits, now)?;
    send(&mut permit, now)?;
    drop(permit);
    assert!(matches!(
        runtime.admit_at(key("one", "a"), &limits, now + Duration::from_secs(60)),
        Err(Refusal::RateLimited)
    ));
    drop(admitted(
        &runtime,
        key("one", "a"),
        &limits,
        now + Duration::from_secs(61),
    )?);
    Ok(())
}
#[test]
fn provider_rejection_does_not_consume_model_quota() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let limits = Limits {
        provider: Some(("*".into(), 1)),
        model: Some(1),
    };
    let pending = admitted(&runtime, key("one", "a"), &limits, now)?;
    assert!(matches!(
        runtime.admit_at(key("one", "b"), &limits, now),
        Err(Refusal::RateLimited)
    ));
    drop(pending);
    drop(admitted(&runtime, key("one", "b"), &limits, now)?);
    Ok(())
}
#[test]
fn exact_credential_account_endpoint_namespace_isolation() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let limits = Limits {
        provider: Some(("*".into(), 1)),
        model: Some(1),
    };
    let mut first = admitted(&runtime, key("tenant-a/credential-v1", "a"), &limits, now)?;
    send(&mut first, now)?;
    let mut keys = vec![
        key("tenant-b/credential-v1", "a"),
        key("tenant-a/credential-v2", "a"),
    ];
    let mut endpoint = key("tenant-a/credential-v1", "a");
    endpoint.endpoint.push('/');
    keys.push(endpoint);
    let mut account = key("tenant-a/credential-v1", "a");
    account.account = Some("second".into());
    keys.push(account);
    for key in keys {
        drop(admitted(&runtime, key, &limits, now)?);
    }
    assert!(matches!(
        runtime.admit_at(key("tenant-a/credential-v1", "a"), &limits, now),
        Err(Refusal::RateLimited)
    ));
    Ok(())
}
#[test]
fn missing_insufficient_stale_and_tied_metrics_preserve_order() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let a = key("one", "a");
    let b = key("one", "b");
    let candidates = vec![(a.clone(), None), (b.clone(), None)];
    let order = |at| {
        runtime
            .order_at(&candidates, Order::Latency, at)
            .map_err(|e| format!("order failed: {e:?}"))
    };
    assert_eq!(order(now)?, vec![0, 1]);
    for _ in 0..MIN_SAMPLES - 1 {
        sample(&runtime, &b, now, Duration::from_secs(1), Some(10))?;
    }
    assert_eq!(order(now + Duration::from_secs(2))?, vec![0, 1]);
    sample(&runtime, &b, now, Duration::from_secs(1), Some(10))?;
    assert_eq!(order(now + Duration::from_secs(2))?, vec![1, 0]);
    for _ in 0..MIN_SAMPLES {
        sample(&runtime, &a, now, Duration::from_secs(1), Some(10))?;
    }
    assert_eq!(order(now + Duration::from_secs(2))?, vec![0, 1]);
    assert_eq!(order(now + FRESH + Duration::from_secs(1))?, vec![0, 1]);
    Ok(())
}
#[test]
fn throughput_requires_output_and_cost_uses_exact_integer_order() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let a = key("one", "a");
    let b = key("one", "b");
    let c = key("one", "c");
    for _ in 0..MIN_SAMPLES {
        sample(&runtime, &a, now, Duration::from_secs(1), None)?;
        sample(&runtime, &b, now, Duration::from_secs(2), Some(100))?;
        sample(&runtime, &c, now, Duration::from_secs(1), Some(100))?;
    }
    let candidates = vec![(a, None), (b, Some(u64::MAX)), (c, Some(u64::MAX - 1))];
    assert_eq!(
        runtime
            .order_at(&candidates, Order::Throughput, now + Duration::from_secs(3))
            .map_err(|e| format!("{e:?}"))?,
        vec![2, 1, 0]
    );
    assert_eq!(
        runtime
            .order_at(&candidates, Order::Cost, now)
            .map_err(|e| format!("{e:?}"))?,
        vec![2, 1, 0]
    );
    assert_eq!(
        runtime
            .order_at(&candidates, Order::Retained, now)
            .map_err(|e| format!("{e:?}"))?,
        vec![0, 1, 2]
    );
    Ok(())
}
#[test]
fn probe_exclusivity_unsent_cancel_and_unknown_sent_cancel() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let key = key("one", "a");
    trip(&runtime, &key, now)?;
    assert!(matches!(
        runtime.admit_at(key.clone(), &Limits::default(), now),
        Err(Refusal::CircuitOpen)
    ));
    let at = now + OPEN;
    let probe = admitted(&runtime, key.clone(), &Limits::default(), at)?;
    assert!(matches!(
        runtime.admit_at(key.clone(), &Limits::default(), at),
        Err(Refusal::CircuitOpen)
    ));
    drop(probe);
    let mut probe = admitted(&runtime, key.clone(), &Limits::default(), at)?;
    send(&mut probe, at)?;
    probe.finish_at(Outcome::Unknown, at);
    assert!(matches!(
        runtime.admit_at(key.clone(), &Limits::default(), at),
        Err(Refusal::CircuitOpen)
    ));
    let mut probe = admitted(&runtime, key.clone(), &Limits::default(), at + OPEN)?;
    send(&mut probe, at + OPEN)?;
    probe.finish_at(
        Outcome::Success {
            output_tokens: None,
        },
        at + OPEN + Duration::from_millis(1),
    );
    drop(admitted(
        &runtime,
        key,
        &Limits::default(),
        at + OPEN + Duration::from_millis(2),
    )?);
    Ok(())
}
#[test]
fn stale_success_cannot_heal_new_circuit_generation() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let key = key("one", "a");
    let mut old = admitted(&runtime, key.clone(), &Limits::default(), now)?;
    send(&mut old, now)?;
    trip(&runtime, &key, now)?;
    old.finish_at(
        Outcome::Success {
            output_tokens: Some(5),
        },
        now + Duration::from_secs(1),
    );
    assert!(matches!(
        runtime.admit_at(
            key.clone(),
            &Limits::default(),
            now + Duration::from_secs(2)
        ),
        Err(Refusal::CircuitOpen)
    ));
    Ok(())
}
#[test]
fn capacity_never_evicts_live_windows() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    for index in 0..MAX_ENTRIES {
        let mut permit = admitted(
            &runtime,
            key("one", &index.to_string()),
            &Limits {
                model: Some(1),
                provider: None,
            },
            now,
        )?;
        send(&mut permit, now)?;
        permit.finish_at(Outcome::Neutral, now);
    }
    assert!(matches!(
        runtime.admit_at(key("one", "new"), &Limits::default(), now),
        Err(Refusal::Capacity)
    ));
    assert!(matches!(
        runtime.admit_at(
            key("one", "0"),
            &Limits {
                model: Some(1),
                provider: None
            },
            now
        ),
        Err(Refusal::RateLimited)
    ));
    drop(admitted(
        &runtime,
        key("one", "new"),
        &Limits::default(),
        now + FRESH,
    )?);
    Ok(())
}
#[test]
fn sample_memory_is_bounded() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let key = key("one", "a");
    for _ in 0..SAMPLES + 10 {
        sample(&runtime, &key, now, Duration::from_millis(1), Some(0))?;
    }
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(
        state
            .targets
            .get(&key)
            .ok_or("missing target")?
            .samples
            .len(),
        SAMPLES
    );
    Ok(())
}

#[test]
fn normal_admission_does_not_walk_other_tenants_windows() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let old = key("other-tenant", "a");
    let mut permit = admitted(&runtime, old.clone(), &Limits::default(), now)?;
    send(&mut permit, now)?;
    permit.finish_at(Outcome::Neutral, now);
    drop(admitted(
        &runtime,
        key("current-tenant", "b"),
        &Limits::default(),
        now + FRESH,
    )?);
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    // Unrelated stale state is untouched until capacity pressure, which makes
    // per-send work independent of the number of retained tenant entries.
    assert_eq!(
        state
            .targets
            .get(&old)
            .ok_or("old target missing")?
            .rate
            .seconds
            .len(),
        1
    );
    Ok(())
}

#[test]
fn capacity_cleanup_preserves_pending_and_half_open_owner() -> TestResult {
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let held = key("one", "held");
    trip(&runtime, &held, now)?;
    let probe = admitted(&runtime, held.clone(), &Limits::default(), now + OPEN)?;
    for index in 0..MAX_ENTRIES - 1 {
        let mut unused = admitted(
            &runtime,
            key("one", &index.to_string()),
            &Limits::default(),
            now,
        )?;
        unused.finish_at(Outcome::Unknown, now);
    }
    drop(admitted(
        &runtime,
        key("one", "new"),
        &Limits::default(),
        now + FRESH,
    )?);
    assert!(matches!(
        runtime.admit_at(held.clone(), &Limits::default(), now + FRESH),
        Err(Refusal::CircuitOpen)
    ));
    drop(probe);
    drop(admitted(&runtime, held, &Limits::default(), now + FRESH)?);
    Ok(())
}
