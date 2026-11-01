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
    for index in 0..runtime.max_entries {
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
    for index in 0..runtime.max_entries - 1 {
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

fn configured(
    entries: usize,
    bytes: usize,
    active: usize,
) -> Result<Arc<Runtime>, Box<dyn std::error::Error>> {
    Ok(Arc::new(Runtime::configured(
        &crate::config::tenancy::capacity::Routing { entries, bytes },
        active,
    )?))
}

fn used(runtime: &Runtime) -> Result<usize, Box<dyn std::error::Error>> {
    runtime
        .state
        .lock()
        .map_err(|_| "poisoned state")?
        .used_bytes()
        .ok_or("byte overflow".into())
}

fn roomy_key() -> Key {
    let mut key = key("one", "a");
    // Small logical key, large retained allocation: admission must charge capacity.
    key.namespace.reserve_exact(32 * 1024);
    key
}

fn provider_limits() -> Limits {
    Limits {
        provider: Some(("*".into(), 1000)),
        model: Some(1000),
    }
}

fn seed(runtime: &Arc<Runtime>, limits: &Limits) -> TestResult {
    let mut permit = admitted(runtime, key("one", "a"), limits, runtime.epoch)?;
    send(&mut permit, runtime.epoch)?;
    permit.finish_at(Outcome::Neutral, runtime.epoch);
    Ok(())
}

#[test]
fn configured_budgets_validate_without_preallocating_maps() -> TestResult {
    let limits = crate::config::tenancy::capacity::Routing::default();
    for (entries, bytes, active) in [
        (0, limits.bytes, 1),
        (1, 0, 1),
        (1, size_of::<Runtime>() - 1, 1),
        (1, limits.bytes, 0),
        (usize::MAX, limits.bytes, 1),
        (1, usize::MAX, 1),
        (1, limits.bytes, usize::MAX),
    ] {
        assert!(configured(entries, bytes, active).is_err());
    }
    let runtime = configured(1_000_000, size_of::<Runtime>(), 1_000_000)?;
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(state.targets.capacity(), 0);
    assert_eq!(state.providers.capacity(), 0);
    assert_eq!(state.used_bytes(), Some(size_of::<Runtime>()));
    drop(state);
    assert!(matches!(
        runtime.admit_at(key("one", "a"), &Limits::default(), runtime.epoch),
        Err(Refusal::Capacity)
    ));
    Ok(())
}

#[test]
fn configured_entry_count_counts_targets_and_providers_at_boundary() -> TestResult {
    let runtime = configured(3, 1024 * 1024, 3)?;
    let now = runtime.epoch;
    let limits = provider_limits();
    let a = admitted(&runtime, key("one", "a"), &limits, now)?;
    assert_eq!(
        runtime
            .state
            .lock()
            .map_err(|_| "poisoned state")?
            .targets
            .len(),
        1
    );
    let b = admitted(&runtime, key("one", "b"), &limits, now)?;
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(state.targets.len() + state.providers.len(), 3);
    drop(state);
    assert!(matches!(
        runtime.admit_at(key("one", "c"), &limits, now),
        Err(Refusal::Capacity)
    ));
    drop((a, b));
    Ok(())
}

#[test]
fn configured_active_count_admits_n_minus_one_n_and_refuses_n_plus_one() -> TestResult {
    let runtime = configured(1, 1024 * 1024, 2)?;
    let now = runtime.epoch;
    let a = admitted(&runtime, key("one", "a"), &Limits::default(), now)?;
    let b = admitted(&runtime, key("one", "a"), &Limits::default(), now)?;
    assert_eq!(
        runtime.state.lock().map_err(|_| "poisoned state")?.active,
        2
    );
    assert!(matches!(
        runtime.admit_at(key("one", "a"), &Limits::default(), now),
        Err(Refusal::Capacity)
    ));
    drop(a);
    drop(admitted(
        &runtime,
        key("one", "a"),
        &Limits::default(),
        now,
    )?);
    drop(b);
    Ok(())
}

#[test]
fn configured_bytes_accept_exact_guard_budget_and_one_more_but_not_one_less() -> TestResult {
    let limits = provider_limits();
    let sizing = configured(2, 1024 * 1024, 2)?;
    seed(&sizing, &limits)?;
    let base = used(&sizing)?;
    let permit = admitted(&sizing, roomy_key(), &limits, sizing.epoch)?;
    let guard_bytes = size_of::<Admission>()
        + permit.key.bytes().ok_or("key byte overflow")?
        + permit
            .provider
            .as_ref()
            .ok_or("missing provider")?
            .bytes()
            .ok_or("provider byte overflow")?;
    assert_eq!(used(&sizing)?, base + guard_bytes);
    drop(permit);
    for bytes in [
        base + guard_bytes - 1,
        base + guard_bytes,
        base + guard_bytes + 1,
    ] {
        let runtime = configured(2, bytes, 2)?;
        seed(&runtime, &limits)?;
        let result = runtime.admit_at(roomy_key(), &limits, runtime.epoch);
        if bytes < base + guard_bytes {
            assert!(matches!(result, Err(Refusal::Capacity)));
            assert_eq!(used(&runtime)?, base);
        } else {
            let permit = result.map_err(|error| format!("admission refused: {error:?}"))?;
            assert_eq!(used(&runtime)?, base + guard_bytes);
            drop(permit);
            assert_eq!(used(&runtime)?, base);
        }
    }
    Ok(())
}

#[test]
fn finished_guard_copies_remain_charged_until_drop() -> TestResult {
    let limits = provider_limits();
    let sizing = configured(2, 1024 * 1024, 2)?;
    seed(&sizing, &limits)?;
    let base = used(&sizing)?;
    let permit = admitted(&sizing, roomy_key(), &limits, sizing.epoch)?;
    let budget = used(&sizing)?;
    drop(permit);
    let runtime = configured(2, budget, 2)?;
    seed(&runtime, &limits)?;
    let mut permit = admitted(&runtime, roomy_key(), &limits, runtime.epoch)?;
    permit.finish_at(Outcome::Unknown, runtime.epoch);
    assert_eq!(used(&runtime)?, budget);
    assert_eq!(
        runtime.state.lock().map_err(|_| "poisoned state")?.active,
        0
    );
    assert!(matches!(
        runtime.admit_at(roomy_key(), &limits, runtime.epoch),
        Err(Refusal::Capacity)
    ));
    drop(permit);
    assert_eq!(used(&runtime)?, base);
    let mut permit = admitted(&runtime, roomy_key(), &limits, runtime.epoch)?;
    send(&mut permit, runtime.epoch)?;
    drop(permit);
    assert_eq!(used(&runtime)?, base);
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(
        state
            .targets
            .get(&key("one", "a"))
            .ok_or("missing target")?
            .rate
            .seconds
            .len(),
        1
    );
    assert_eq!(
        state
            .providers
            .values()
            .next()
            .ok_or("missing provider")?
            .seconds
            .len(),
        1
    );
    Ok(())
}

#[test]
fn configured_entries_can_exceed_2048() -> TestResult {
    let runtime = configured(2049, 64 * 1024 * 1024, 1)?;
    let now = runtime.epoch;
    for index in 0..2049 {
        let mut permit = admitted(
            &runtime,
            key("one", &index.to_string()),
            &Limits::default(),
            now,
        )?;
        send(&mut permit, now)?;
        permit.finish_at(Outcome::Neutral, now);
    }
    assert_eq!(
        runtime
            .state
            .lock()
            .map_err(|_| "poisoned state")?
            .targets
            .len(),
        2049
    );
    assert!(matches!(
        runtime.admit_at(key("one", "overflow"), &Limits::default(), now),
        Err(Refusal::Capacity)
    ));
    Ok(())
}

#[test]
fn configured_owners_can_exceed_4096() -> TestResult {
    let runtime = configured(1, 64 * 1024 * 1024, 4097)?;
    let now = runtime.epoch;
    let mut permits = Vec::new();
    for _ in 0..4097 {
        permits.push(admitted(
            &runtime,
            key("one", "a"),
            &Limits::default(),
            now,
        )?);
    }
    assert_eq!(
        runtime.state.lock().map_err(|_| "poisoned state")?.active,
        4097
    );
    assert!(matches!(
        runtime.admit_at(key("one", "a"), &Limits::default(), now),
        Err(Refusal::Capacity)
    ));
    drop(permits);
    assert_eq!(
        runtime.state.lock().map_err(|_| "poisoned state")?.active,
        0
    );
    Ok(())
}

#[test]
fn active_sent_owner_pins_provider_after_rpm_expiry() -> TestResult {
    let runtime = configured(2, 1024 * 1024, 2)?;
    let now = runtime.epoch;
    let limits = provider_limits();
    let mut permit = admitted(&runtime, key("one", "a"), &limits, now)?;
    send(&mut permit, now)?;
    assert!(matches!(
        runtime.admit_at(key("other", "b"), &limits, now + FRESH),
        Err(Refusal::Capacity)
    ));
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(state.targets.len(), 1);
    assert_eq!(state.providers.len(), 1);
    let rate = state.providers.values().next().ok_or("missing provider")?;
    assert_eq!(rate.active, 1);
    assert_eq!(rate.pending, 0);
    assert!(rate.seconds.is_empty());
    drop(state);
    permit.finish_at(Outcome::Neutral, now + FRESH);
    drop(permit);
    drop(admitted(
        &runtime,
        key("other", "b"),
        &limits,
        now + FRESH + FRESH,
    )?);
    Ok(())
}

#[test]
fn byte_pressure_cannot_evict_an_active_sent_owner() -> TestResult {
    let limits = provider_limits();
    let sizing = configured(8, 1024 * 1024, 2)?;
    seed(&sizing, &limits)?;
    let permit = admitted(&sizing, roomy_key(), &limits, sizing.epoch)?;
    let budget = used(&sizing)?;
    drop(permit);
    let runtime = configured(8, budget, 2)?;
    seed(&runtime, &limits)?;
    let mut permit = admitted(&runtime, roomy_key(), &limits, runtime.epoch)?;
    send(&mut permit, runtime.epoch)?;
    assert!(matches!(
        runtime.admit_at(key("other", "b"), &limits, runtime.epoch + FRESH),
        Err(Refusal::Capacity)
    ));
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(state.targets.len() + state.providers.len(), 2);
    assert_eq!(state.used_bytes(), Some(budget));
    assert_eq!(
        state
            .providers
            .values()
            .next()
            .ok_or("missing provider")?
            .active,
        1
    );
    drop(state);
    drop(permit);
    Ok(())
}

#[test]
fn byte_pressure_reclaims_only_expired_unowned_entries() -> TestResult {
    let runtime = configured(8, 32 * 1024, 2)?;
    let now = runtime.epoch;
    let mut permit = admitted(
        &runtime,
        key("one", &"a".repeat(9000)),
        &Limits::default(),
        now,
    )?;
    send(&mut permit, now)?;
    permit.finish_at(Outcome::Neutral, now);
    drop(permit);
    assert!(matches!(
        runtime.admit_at(key("one", &"b".repeat(9000)), &Limits::default(), now),
        Err(Refusal::Capacity)
    ));
    assert_eq!(
        runtime
            .state
            .lock()
            .map_err(|_| "poisoned state")?
            .targets
            .len(),
        1
    );
    drop(admitted(
        &runtime,
        key("one", &"b".repeat(9000)),
        &Limits::default(),
        now + FRESH,
    )?);
    assert!(used(&runtime)? <= runtime.max_bytes);
    Ok(())
}

#[test]
fn future_rpm_buckets_and_samples_fit_at_full_byte_budget() -> TestResult {
    let limits = provider_limits();
    let sizing = configured(2, 1024 * 1024, 2)?;
    seed(&sizing, &limits)?;
    let permit = admitted(&sizing, roomy_key(), &limits, sizing.epoch)?;
    let budget = used(&sizing)?;
    drop(permit);
    let runtime = configured(2, budget, 2)?;
    seed(&runtime, &limits)?;
    let now = runtime.epoch;
    let capacities = {
        let state = runtime.state.lock().map_err(|_| "poisoned state")?;
        let target = state
            .targets
            .get(&key("one", "a"))
            .ok_or("missing target")?;
        let provider = state.providers.values().next().ok_or("missing provider")?;
        assert!(
            state.bytes
                >= target.rate.seconds.capacity() * size_of::<(u64, u64)>()
                    + target.samples.capacity() * size_of::<Sample>()
                    + provider.seconds.capacity() * size_of::<(u64, u64)>()
        );
        (
            target.rate.seconds.capacity(),
            target.samples.capacity(),
            provider.seconds.capacity(),
        )
    };
    for tick in 0..RATE_BUCKETS {
        let at = now + Duration::from_secs(tick as u64);
        let mut permit = admitted(&runtime, roomy_key(), &limits, at)?;
        assert_eq!(used(&runtime)?, budget);
        assert!(matches!(
            runtime.admit_at(key("one", "a"), &limits, at),
            Err(Refusal::Capacity)
        ));
        send(&mut permit, at)?;
        permit.finish_at(
            Outcome::Success {
                output_tokens: Some(1),
            },
            at + Duration::from_millis(1),
        );
        assert_eq!(used(&runtime)?, budget);
    }
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    let target = state
        .targets
        .get(&key("one", "a"))
        .ok_or("missing target")?;
    let provider = state.providers.values().next().ok_or("missing provider")?;
    assert_eq!(target.rate.seconds.len(), RATE_BUCKETS);
    assert_eq!(provider.seconds.len(), RATE_BUCKETS);
    assert_eq!(target.samples.len(), SAMPLES);
    assert_eq!(
        (
            target.rate.seconds.capacity(),
            target.samples.capacity(),
            provider.seconds.capacity()
        ),
        capacities
    );
    Ok(())
}

#[test]
fn map_capacity_metadata_remains_charged_after_reclamation() -> TestResult {
    let runtime = configured(3, 1024 * 1024, 2)?;
    let now = runtime.epoch;
    let held = admitted(&runtime, key("one", "held"), &Limits::default(), now)?;
    for model in ["old-a", "old-b"] {
        let mut permit = admitted(&runtime, key("one", model), &Limits::default(), now)?;
        permit.finish_at(Outcome::Unknown, now);
    }
    let allocated = runtime
        .state
        .lock()
        .map_err(|_| "poisoned state")?
        .target_bytes;
    let fresh = admitted(
        &runtime,
        key("one", "fresh"),
        &Limits::default(),
        now + FRESH,
    )?;
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(state.targets.len(), 2);
    assert_eq!(state.target_bytes, allocated);
    assert!(allocated > state.targets.len() * size_of::<(Key, Entry)>());
    drop(state);
    drop((held, fresh));
    Ok(())
}

#[test]
fn widened_pending_and_sent_counts_do_not_wrap() -> TestResult {
    let mut rate = Rate::new().map_err(|error| format!("rate allocation: {error:?}"))?;
    rate.pending = usize::MAX;
    rate.seconds.push_back((0, u64::MAX));
    assert!(!rate.available(u32::MAX));
    rate.sent(0);
    assert_eq!(rate.pending, usize::MAX - 1);
    assert_eq!(rate.seconds.back(), Some(&(0, u64::MAX)));
    assert!(!rate.available(u32::MAX));
    Ok(())
}

#[test]
fn pending_overflow_refuses_before_reserving_either_quota() -> TestResult {
    let runtime = configured(2, 1024 * 1024, 2)?;
    let limits = provider_limits();
    seed(&runtime, &limits)?;
    let baseline = used(&runtime)?;
    runtime
        .state
        .lock()
        .map_err(|_| "poisoned state")?
        .targets
        .get_mut(&key("one", "a"))
        .ok_or("missing target")?
        .rate
        .pending = usize::MAX;
    // An unlimited revision must still track pending model calls safely.
    let unlimited = Limits {
        model: None,
        ..limits
    };
    assert!(matches!(
        runtime.admit_at(key("one", "a"), &unlimited, runtime.epoch),
        Err(Refusal::Capacity)
    ));
    let state = runtime.state.lock().map_err(|_| "poisoned state")?;
    assert_eq!(state.active, 0);
    assert_eq!(state.used_bytes(), Some(baseline));
    assert_eq!(
        state
            .providers
            .values()
            .next()
            .ok_or("missing provider")?
            .pending,
        0
    );
    Ok(())
}

#[test]
fn out_of_order_send_times_cannot_grow_reserved_window() -> TestResult {
    let mut rate = Rate::new().map_err(|error| format!("rate allocation: {error:?}"))?;
    let capacity = rate.seconds.capacity();
    for tick in 0..RATE_BUCKETS as u64 {
        rate.sent(tick);
        rate.sent(0);
    }
    assert_eq!(rate.seconds.len(), RATE_BUCKETS);
    assert_eq!(rate.seconds.capacity(), capacity);
    assert_eq!(
        rate.seconds.iter().map(|(_, count)| count).sum::<u64>(),
        2 * RATE_BUCKETS as u64
    );
    Ok(())
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
#[test]
fn observation_copies_existing_routing_accounting_without_reclamation() -> TestResult {
    use crate::telemetry::receipt_latency::capacity::ReadError;
    let runtime = Arc::new(Runtime::default());
    let now = runtime.epoch;
    let initial = runtime.observed().map_err(|e| format!("{e:?}"))?;
    let limits = Limits {
        provider: Some(("*".into(), 7)),
        model: Some(4),
    };
    let permit = admitted(&runtime, key("observation", "model"), &limits, now)?;
    let active = runtime.observed().map_err(|e| format!("{e:?}"))?;
    assert_eq!(active.active, 1);
    assert!(active.targets > 0 && active.bytes > initial.bytes);
    assert_eq!(active.providers, 1);
    let held = runtime.state.lock().map_err(|_| "routing lock")?;
    assert_eq!(runtime.observed(), Err(ReadError::Unavailable));
    drop(held);
    assert_eq!(runtime.observed().map_err(|e| format!("{e:?}"))?, active);
    drop(permit);
    let released = runtime.observed().map_err(|e| format!("{e:?}"))?;
    assert_eq!(released.active, 0);
    assert!(released.bytes <= active.bytes);
    Ok(())
}
