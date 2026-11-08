//! Deterministic state tests; socket dispatch is covered separately.
use super::*;
use bitrouter_ai::tenancy::admission_v2::AdmissionProfileV2;

fn grouped(rpm: u32) -> Result<AdmissionProfileV2> {
    let local = rules(rpm)?;
    Ok(AdmissionProfileV2 {
        schema_version: 2,
        rate: local.rate,
        observe_rate_group_id: None,
        health: local.health,
        health_group_id: Some("circuit".try_into()?),
        fallback: local.fallback,
    })
}

#[test]
fn health_group_does_not_partition_by_rate_group_presence_observation_or_fallback() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let a = reference("a")?;
    let b = reference("b")?;
    let c = reference("c")?;
    let first = grouped(100)?;
    let mut second = grouped(7)?;
    second.rate.as_mut().context("rate")?.rate_group_id = "other-window".try_into()?;
    second.fallback = FallbackRule::Stop;
    let mut third = grouped(3)?;
    third.rate = None;
    third.observe_rate_group_id = Some("shared".try_into()?);
    let now = Instant::now();
    take(&runtime, &installation, &a, &first, now)?.finish(HealthOutcome::Persistent, now);
    for (member, profile) in [(&b, &second), (&c, &third)] {
        assert_eq!(
            view(&runtime, &installation, member, profile, now)?.health,
            HealthObservation::Blocked
        );
        assert!(matches!(
            runtime.admit_at(&installation, member, profile, now),
            Err(Denial::Circuit)
        ));
    }
    assert_eq!(
        view(&runtime, &installation, &b, &second, now)?
            .rate
            .context("second rate")?
            .shared_count,
        0
    );
    let observed = view(&runtime, &installation, &c, &third, now)?;
    assert!(observed.rate.is_none());
    assert_eq!(observed.observed_shared_count, Some(1));
    Ok(())
}

fn take(
    runtime: &Arc<Runtime>,
    installation: &InstallationId,
    member: &ResourceRef,
    profile: &AdmissionProfileV2,
    now: Instant,
) -> Result<super::super::state::Ticket> {
    runtime
        .admit_at(installation, member, profile, now)
        .map_err(|e| anyhow::anyhow!("unexpected refusal: {e:?}"))
}

fn view(
    runtime: &Runtime,
    installation: &InstallationId,
    member: &ResourceRef,
    profile: &AdmissionProfileV2,
    now: Instant,
) -> Result<super::super::state::View> {
    runtime
        .observe(installation, member, profile, now)
        .map_err(|e| anyhow::anyhow!("observation: {e:?}"))
}

#[test]
fn distinct_member_ceilings_share_rate_and_distributed_health_failures() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let a = reference("alias-a")?;
    let b = reference("alias-b")?;
    let now = Instant::now();
    let low = grouped(1)?;
    let high = grouped(3)?;
    take(&runtime, &installation, &a, &low, now)?
        .finish(HealthOutcome::Transient { retry_after: 0 }, now);
    assert!(matches!(
        runtime.admit_at(&installation, &a, &low, now),
        Err(Denial::Rate)
    ));
    assert_eq!(
        view(&runtime, &installation, &b, &high, now)?.health,
        HealthObservation::Healthy
    );
    take(&runtime, &installation, &b, &high, now)?
        .finish(HealthOutcome::Transient { retry_after: 0 }, now);
    for (member, profile) in [(&a, &low), (&b, &high)] {
        let observed = view(&runtime, &installation, member, profile, now)?;
        let rate = observed.rate.context("rate")?;
        assert_eq!(rate.shared_count, 2);
        assert_eq!(
            rate.member_rpm,
            profile.rate.as_ref().context("profile rate")?.member_rpm
        );
        assert_eq!(observed.health, HealthObservation::Blocked);
        assert_eq!(
            observed.availability,
            AvailabilityObservation::CircuitBlocked
        );
        assert!(matches!(
            runtime.admit_at(&installation, member, profile, now),
            Err(Denial::Circuit)
        ));
    }
    Ok(())
}

#[test]
fn tagged_groups_isolate_local_profile_pool_installation_and_other_group() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let a = reference("circuit")?;
    let b = reference("sibling")?;
    let mut profile = grouped(100)?;
    profile.rate = None;
    let now = Instant::now();
    take(&runtime, &installation, &a, &profile, now)?.finish(HealthOutcome::Persistent, now);
    assert_eq!(
        view(&runtime, &installation, &b, &profile, now)?.health,
        HealthObservation::Blocked
    );
    let mut local = rules(100)?;
    local.rate = None;
    assert_eq!(
        runtime
            .observe(&installation, &a, &local, now)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .health,
        HealthObservation::Healthy
    );
    let mut other_pool = a.clone();
    other_pool.owner_id = "pool-b".try_into()?;
    assert_eq!(
        view(&runtime, &installation, &other_pool, &profile, now)?.health,
        HealthObservation::Healthy
    );
    assert_eq!(
        view(
            &runtime,
            &"other-installation".try_into()?,
            &a,
            &profile,
            now
        )?
        .health,
        HealthObservation::Healthy
    );
    let mut other_group = profile.clone();
    other_group.health_group_id = Some("other".try_into()?);
    take(&runtime, &installation, &a, &other_group, now)?.finish(HealthOutcome::Success, now);
    admitted(&runtime, &installation, &a, &local, now)?.finish(HealthOutcome::Persistent, now);
    assert_eq!(
        runtime
            .observe(&installation, &b, &local, now)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .health,
        HealthObservation::Healthy
    );
    assert_eq!(
        view(&runtime, &installation, &b, &profile, now)?.health,
        HealthObservation::Blocked
    );
    Ok(())
}

#[test]
fn cross_member_probe_race_is_exclusive_and_drop_releases_only_its_lease() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation: InstallationId = "installation".try_into()?;
    let mut profile = grouped(100)?;
    profile.rate = None;
    let now = Instant::now();
    take(
        &runtime,
        &installation,
        &reference("initial")?,
        &profile,
        now,
    )?
    .finish(HealthOutcome::Persistent, now);
    let ready = now + Duration::from_secs(30);
    let barrier = Arc::new(Barrier::new(16));
    let mut workers = Vec::new();
    for n in 0..16 {
        let (runtime, installation, profile, barrier) = (
            runtime.clone(),
            installation.clone(),
            profile.clone(),
            barrier.clone(),
        );
        let member = reference(&format!("member-{n}"))?;
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            runtime.admit_at(&installation, &member, &profile, ready)
        }));
    }
    let mut tickets = Vec::new();
    for worker in workers {
        match worker
            .join()
            .map_err(|_| anyhow::anyhow!("probe worker failed"))?
        {
            Ok(ticket) => tickets.push(ticket),
            Err(error) => assert_eq!(error, Denial::Circuit),
        }
    }
    assert_eq!(tickets.len(), 1);
    assert_eq!(
        view(
            &runtime,
            &installation,
            &reference("reader")?,
            &profile,
            ready
        )?
        .health,
        HealthObservation::Blocked
    );
    drop(tickets);
    assert_eq!(
        view(
            &runtime,
            &installation,
            &reference("reader")?,
            &profile,
            ready
        )?
        .health,
        HealthObservation::ProbeReady
    );
    take(
        &runtime,
        &installation,
        &reference("next")?,
        &profile,
        ready,
    )?
    .finish(HealthOutcome::Success, ready);
    assert_eq!(
        view(
            &runtime,
            &installation,
            &reference("reader")?,
            &profile,
            ready
        )?
        .health,
        HealthObservation::Healthy
    );
    Ok(())
}

#[test]
fn late_ordinary_and_expired_cross_member_tickets_cannot_complete_new_probe() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let a = reference("a")?;
    let b = reference("b")?;
    let mut profile = grouped(100)?;
    profile.rate = None;
    let now = Instant::now();
    let late = take(&runtime, &installation, &a, &profile, now)?;
    take(&runtime, &installation, &b, &profile, now)?.finish(HealthOutcome::Persistent, now);
    let ready = now + Duration::from_secs(30);
    let expired = take(&runtime, &installation, &a, &profile, ready)?;
    let replaced = ready + Duration::from_secs(5);
    let current = take(&runtime, &installation, &b, &profile, replaced)?;
    late.finish(HealthOutcome::Success, replaced);
    expired.finish(HealthOutcome::Persistent, replaced);
    assert_eq!(
        view(&runtime, &installation, &a, &profile, replaced)?.health,
        HealthObservation::Blocked
    );
    assert!(matches!(
        runtime.admit_at(&installation, &a, &profile, replaced),
        Err(Denial::Circuit)
    ));
    current.finish(HealthOutcome::Neutral, replaced);
    assert_eq!(
        view(&runtime, &installation, &a, &profile, replaced)?.health,
        HealthObservation::ProbeReady
    );
    take(&runtime, &installation, &a, &profile, replaced)?.finish(HealthOutcome::Success, replaced);
    assert_eq!(
        view(&runtime, &installation, &b, &profile, replaced)?.health,
        HealthObservation::Healthy
    );
    Ok(())
}

#[test]
fn rate_denied_probe_releases_group_and_rpm_rotation_keeps_captured_identity() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let mut a = reference("a")?;
    let b = reference("b")?;
    let low = grouped(1)?;
    let high = grouped(3)?;
    let now = Instant::now();
    take(&runtime, &installation, &a, &low, now)?.finish(HealthOutcome::Persistent, now);
    let ready = now + Duration::from_secs(30);
    assert!(matches!(
        runtime.admit_at(&installation, &a, &low, ready),
        Err(Denial::Rate)
    ));
    assert_eq!(
        view(&runtime, &installation, &b, &high, ready)?.health,
        HealthObservation::ProbeReady
    );
    a.revision_id = "rotated".try_into()?;
    let mut zero = low.clone();
    zero.rate.as_mut().context("rate")?.member_rpm = 0;
    assert!(matches!(
        runtime.admit_at(&installation, &a, &zero, ready),
        Err(Denial::Rate)
    ));
    let ticket = take(&runtime, &installation, &b, &high, ready)?;
    assert_eq!(
        view(&runtime, &installation, &a, &zero, ready)?.health,
        HealthObservation::Blocked
    );
    ticket.finish(HealthOutcome::Success, ready);
    assert_eq!(
        view(&runtime, &installation, &a, &high, ready)?.health,
        HealthObservation::Healthy
    );
    assert_eq!(
        view(&runtime, &installation, &a, &high, ready)?
            .rate
            .context("rate")?
            .shared_count,
        2
    );
    assert_eq!(
        view(&runtime, &installation, &a, &zero, ready)?.availability,
        AvailabilityObservation::RateLimited
    );
    Ok(())
}

#[test]
fn disabled_health_bypass_reads_count_but_never_changes_group_health_or_rate() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let member = reference("member")?;
    let bypass_member = reference("caller-owned")?;
    let profile = grouped(3)?;
    let mut bypass = profile.clone();
    bypass.rate = None;
    bypass.health = None;
    bypass.health_group_id = None;
    bypass.observe_rate_group_id = Some("shared".try_into()?);
    let now = Instant::now();
    take(&runtime, &installation, &member, &profile, now)?.finish(HealthOutcome::Persistent, now);
    for outcome in [
        HealthOutcome::Success,
        HealthOutcome::Persistent,
        HealthOutcome::Transient {
            retry_after: u64::MAX,
        },
        HealthOutcome::Neutral,
    ] {
        take(&runtime, &installation, &bypass_member, &bypass, now)?.finish(outcome, now);
        let read = view(&runtime, &installation, &bypass_member, &bypass, now)?;
        assert!(read.rate.is_none());
        assert_eq!(read.observed_shared_count, Some(1));
        assert_eq!(read.health, HealthObservation::Disabled);
        assert_eq!(read.availability, AvailabilityObservation::Eligible);
        let shared = view(&runtime, &installation, &member, &profile, now)?;
        assert_eq!(shared.health, HealthObservation::Blocked);
        assert_eq!(shared.rate.context("rate")?.shared_count, 1);
    }
    Ok(())
}

#[test]
fn observations_do_not_allocate_and_capacity_counts_group_not_members() -> Result<()> {
    let runtime = Arc::new(Runtime::default());
    let installation = "installation".try_into()?;
    let member = reference("member")?;
    let sibling = reference("sibling")?;
    let mut profile = grouped(100)?;
    profile.rate = None;
    let now = Instant::now();
    for n in 0..crate::tenancy::operational::MAX_RUNTIME_KEYS {
        profile.health_group_id = Some(format!("read-{n}").try_into()?);
        assert_eq!(
            view(&runtime, &installation, &member, &profile, now)?.availability,
            AvailabilityObservation::Eligible
        );
    }
    for n in 0..crate::tenancy::operational::MAX_RUNTIME_KEYS {
        profile.health_group_id = Some(format!("write-{n}").try_into()?);
        take(&runtime, &installation, &member, &profile, now)?.finish(HealthOutcome::Neutral, now);
    }
    profile.health_group_id = Some("write-0".try_into()?);
    assert_eq!(
        view(&runtime, &installation, &sibling, &profile, now)?.availability,
        AvailabilityObservation::Eligible
    );
    take(&runtime, &installation, &sibling, &profile, now)?.finish(HealthOutcome::Persistent, now);
    assert_eq!(
        view(&runtime, &installation, &member, &profile, now)?.health,
        HealthObservation::Blocked
    );
    profile.health_group_id = Some("read-0".try_into()?);
    assert_eq!(
        view(&runtime, &installation, &member, &profile, now)?.availability,
        AvailabilityObservation::CapacityDenied
    );
    assert!(matches!(
        runtime.admit_at(&installation, &member, &profile, now),
        Err(Denial::Capacity)
    ));
    // V1 uses its tagged local key and cannot reuse the identically spelled group slot.
    let mut local = rules(100)?;
    local.rate = None;
    assert!(matches!(
        runtime.admit_at(&installation, &reference("write-0")?, &local, now),
        Err(Denial::Capacity)
    ));
    Ok(())
}
