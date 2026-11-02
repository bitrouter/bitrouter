use std::{
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    admission::{
        AdmissionProfileV1, AvailabilityObservation, FallbackRule, HealthObservation,
        HealthProfileV1, HealthRules, RateProfileV1,
    },
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::InstallationId,
};

use super::{
    health::Health,
    rate::Rates,
    state::{Denial, HealthOutcome, Runtime},
};

fn reference(name: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: "pool-a".try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: name.try_into()?,
        revision_id: "r1".try_into()?,
    })
}

fn rules(rpm: u32) -> Result<AdmissionProfileV1> {
    Ok(AdmissionProfileV1 {
        schema_version: 1,
        observe_rate_group_id: None,
        rate: Some(RateProfileV1 {
            rate_group_id: "shared".try_into()?,
            member_rpm: rpm,
        }),
        health: Some(HealthProfileV1 {
            rules: HealthRules::AvailabilityV1,
            failure_threshold: 2,
            transient_cooldown_seconds: 10,
            persistent_cooldown_seconds: 30,
            probe_lease_seconds: 5,
        }),
        fallback: FallbackRule::OrderedTerminalRejectionsV1,
    })
}

fn admitted(
    runtime: &Arc<Runtime>,
    installation: &InstallationId,
    profile: &ResourceRef,
    rules: &AdmissionProfileV1,
    now: Instant,
) -> Result<super::state::Ticket> {
    runtime
        .admit_at(installation, profile, rules, now)
        .map_err(|error| anyhow::anyhow!("unexpected refusal: {error:?}"))
}

#[test]
fn shared_rate_keeps_exact_sixty_second_boundary_and_member_ceiling() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation-a".try_into()?;
    let now = Instant::now();
    let first = reference("first")?;
    let second = reference("second")?;
    admitted(&runtime, &installation, &first, &rules(2)?, now)?.finish(HealthOutcome::Neutral, now);
    assert!(matches!(
        runtime.admit_at(&installation, &second, &rules(1)?, now),
        Err(Denial::Rate)
    ));
    let exact = now + Duration::from_secs(60);
    assert!(matches!(
        runtime.admit_at(&installation, &second, &rules(1)?, exact),
        Err(Denial::Rate)
    ));
    admitted(
        &runtime,
        &installation,
        &second,
        &rules(1)?,
        exact + Duration::from_nanos(1),
    )?
    .finish(HealthOutcome::Neutral, exact);
    Ok(())
}

#[test]
fn rpm_rotation_zero_and_cancel_preserve_charged_history() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation-a".try_into()?;
    let now = Instant::now();
    let mut profile = reference("profile")?;
    drop(admitted(
        &runtime,
        &installation,
        &profile,
        &rules(500)?,
        now,
    )?);
    profile.revision_id = "r2".try_into()?;
    assert!(matches!(
        runtime.admit_at(&installation, &profile, &rules(0)?, now),
        Err(Denial::Rate)
    ));
    profile.revision_id = "r3".try_into()?;
    assert!(matches!(
        runtime.admit_at(&installation, &profile, &rules(1)?, now),
        Err(Denial::Rate)
    ));
    let view = runtime
        .observe(&installation, &profile, &rules(500)?, now)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(view.rate.context("rate missing")?.shared_count, 1);
    Ok(())
}

#[test]
fn shared_limit_race_admits_once_and_owner_installation_are_independent() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let barrier = Arc::new(Barrier::new(16));
    let installation: InstallationId = "installation-a".try_into()?;
    let profile = reference("profile")?;
    let rules = rules(1)?;
    let now = Instant::now();
    let workers = (0..16)
        .map(|_| {
            let (runtime, barrier, installation, profile, rules) = (
                runtime.clone(),
                barrier.clone(),
                installation.clone(),
                profile.clone(),
                rules.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                runtime.admit(&installation, &profile, &rules).is_ok()
            })
        })
        .collect::<Vec<_>>();
    let mut admitted_count = 0;
    for worker in workers {
        admitted_count += usize::from(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("rate worker failed"))?,
        );
    }
    assert_eq!(admitted_count, 1);
    let mut other = profile.clone();
    other.owner_id = "pool-b".try_into()?;
    drop(admitted(&runtime, &installation, &other, &rules, now)?);
    drop(admitted(
        &runtime,
        &"installation-b".try_into()?,
        &profile,
        &rules,
        now,
    )?);
    Ok(())
}

#[test]
fn explicit_read_only_group_observes_across_profiles_without_charge_or_probe() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation-a".try_into()?;
    let platform = reference("rate-member")?;
    let observer = reference("read-only-member")?;
    let now = Instant::now();
    let rules = rules(100)?;
    let mut read_only = rules.clone();
    read_only.rate = None;
    read_only.health = None;
    read_only.observe_rate_group_id = Some("shared".try_into()?);
    let view = runtime
        .observe(&installation, &observer, &read_only, now)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(view.observed_shared_count, Some(0));
    admitted(&runtime, &installation, &platform, &rules, now)?
        .finish(HealthOutcome::Persistent, now);
    let ready = now + Duration::from_secs(30);
    for _ in 0..16 {
        let view = runtime
            .observe(&installation, &observer, &read_only, ready)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(view.observed_shared_count, Some(1));
        assert!(view.rate.is_none());
        assert_eq!(view.health, HealthObservation::Disabled);
        assert_eq!(view.availability, AvailabilityObservation::Eligible);
        drop(admitted(
            &runtime,
            &installation,
            &observer,
            &read_only,
            ready,
        )?);
    }
    let view = runtime
        .observe(&installation, &platform, &rules, ready)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(view.rate.context("rate")?.shared_count, 1);
    assert_eq!(view.health, HealthObservation::ProbeReady);
    let mut foreign = observer.clone();
    foreign.owner_id = "other-owner".try_into()?;
    assert_eq!(
        runtime
            .observe(&installation, &foreign, &read_only, ready)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .observed_shared_count,
        Some(0)
    );
    assert_eq!(
        runtime
            .observe(
                &"other-installation".try_into()?,
                &observer,
                &read_only,
                ready
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .observed_shared_count,
        Some(0)
    );
    assert_eq!(
        runtime
            .observe(
                &installation,
                &observer,
                &read_only,
                now + Duration::from_secs(60)
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .observed_shared_count,
        Some(1)
    );
    assert_eq!(
        runtime
            .observe(
                &installation,
                &observer,
                &read_only,
                now + Duration::from_secs(60) + Duration::from_nanos(1)
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .observed_shared_count,
        Some(0)
    );
    read_only.observe_rate_group_id = None;
    assert!(
        runtime
            .observe(&installation, &observer, &read_only, ready)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .observed_shared_count
            .is_none()
    );
    Ok(())
}

#[test]
fn a_read_snapshot_cannot_split_shared_counts_across_concurrent_admission() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let runtime = Arc::new(Runtime::default());
    let installation: InstallationId = "installation-a".try_into()?;
    let member = reference("member")?;
    let sibling = reference("sibling")?;
    let rules = rules(100)?;
    let snapshot = runtime.snapshot().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let first = snapshot.observe(&installation, &member, &rules);
    let barrier = Arc::new(Barrier::new(2));
    let completed = Arc::new(AtomicBool::new(false));
    let worker = {
        let (runtime, installation, member, rules, barrier, completed) = (
            runtime.clone(),
            installation.clone(),
            member.clone(),
            rules.clone(),
            barrier.clone(),
            completed.clone(),
        );
        std::thread::spawn(move || {
            barrier.wait();
            let result = runtime.admit(&installation, &member, &rules);
            completed.store(true, Ordering::SeqCst);
            result.is_ok()
        })
    };
    barrier.wait();
    let second = snapshot.observe(&installation, &sibling, &rules);
    assert_eq!(first.rate.context("first count")?.shared_count, 0);
    assert_eq!(second.rate.context("second count")?.shared_count, 0);
    assert!(!completed.load(Ordering::SeqCst));
    drop(snapshot);
    ensure!(
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("admission worker"))?
    );
    let after = runtime.snapshot().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(
        after
            .observe(&installation, &sibling, &rules)
            .rate
            .context("after count")?
            .shared_count,
        1
    );
    Ok(())
}

#[test]
fn rate_refusal_releases_only_the_new_probe_and_never_refunds() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation-a".try_into()?;
    let profile = reference("profile")?;
    let now = Instant::now();
    let mut rules = rules(2)?;
    admitted(&runtime, &installation, &profile, &rules, now)?
        .finish(HealthOutcome::Persistent, now);
    let ready = now + Duration::from_secs(30);
    rules.rate.as_mut().context("rate missing")?.member_rpm = 0;
    assert!(matches!(
        runtime.admit_at(&installation, &profile, &rules, ready),
        Err(Denial::Rate)
    ));
    let view = runtime
        .observe(&installation, &profile, &rules, ready)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(view.health, HealthObservation::ProbeReady);
    assert_eq!(view.rate.context("rate missing")?.shared_count, 1);
    rules.rate.as_mut().context("rate missing")?.member_rpm = 2;
    let probe = admitted(&runtime, &installation, &profile, &rules, ready)?;
    assert!(matches!(
        runtime.admit_at(&installation, &profile, &rules, ready),
        Err(Denial::Circuit)
    ));
    probe.finish(HealthOutcome::Success, ready);
    assert!(matches!(
        runtime.admit_at(&installation, &profile, &rules, ready),
        Err(Denial::Rate)
    ));
    Ok(())
}

#[test]
fn stale_probe_success_failure_and_release_cannot_change_new_owner() -> Result<()> {
    for outcome in [
        HealthOutcome::Success,
        HealthOutcome::Persistent,
        HealthOutcome::Neutral,
    ] {
        let mut health = Health::default();
        let rules = rules(1)?.health.context("health missing")?;
        let now = Instant::now();
        let mut sequence = 0;
        health
            .claim(&rules, &mut sequence, now)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        health.finish(None, &rules, HealthOutcome::Persistent, now);
        let ready = now + Duration::from_secs(30);
        let old = health
            .claim(&rules, &mut sequence, ready)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let replacement_at = ready + Duration::from_secs(5);
        let new = health
            .claim(&rules, &mut sequence, replacement_at)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_ne!(old, new);
        health.finish(old, &rules, outcome, replacement_at);
        assert_eq!(health.observe(replacement_at), HealthObservation::Blocked);
        health.finish(new, &rules, HealthOutcome::Success, replacement_at);
        assert_eq!(health.observe(replacement_at), HealthObservation::Healthy);
    }
    Ok(())
}

#[test]
fn maximum_retry_after_never_panics_wraps_or_shortens_cooldown() -> Result<()> {
    let mut health = Health::default();
    let mut rules = rules(1)?.health.context("health missing")?;
    rules.failure_threshold = 1;
    let now = Instant::now();
    health
        .claim(&rules, &mut 0, now)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    health.finish(
        None,
        &rules,
        HealthOutcome::Transient {
            retry_after: u64::MAX,
        },
        now,
    );
    assert_eq!(
        health.observe(now + Duration::from_secs(86_400 * 365)),
        HealthObservation::Blocked
    );
    Ok(())
}

#[test]
fn capacity_never_evicts_live_rate_history_and_reads_do_not_claim() -> Result<()> {
    let mut rates = Rates::bounded(1);
    let now = Instant::now();
    let key: super::rate::Key = (
        "installation-a".try_into()?,
        "pool-a".try_into()?,
        "one".try_into()?,
    );
    let other = (key.0.clone(), key.1.clone(), "two".try_into()?);
    rates
        .admit(key.clone(), 100, now)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert!(matches!(
        rates.admit(other.clone(), 100, now),
        Err(Denial::Capacity)
    ));
    assert_eq!(rates.count(&key, now), 1);
    rates
        .admit(other, 100, now + Duration::from_secs(61))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let runtime = Arc::new(Runtime::default());
    let profile = reference("profile")?;
    for _ in 0..100 {
        let view = runtime
            .observe(&key.0, &profile, &rules(1)?, now)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ensure!(view.availability == AvailabilityObservation::Eligible);
        assert_eq!(view.rate.context("rate missing")?.shared_count, 0);
    }
    drop(admitted(&runtime, &key.0, &profile, &rules(1)?, now)?);
    Ok(())
}
