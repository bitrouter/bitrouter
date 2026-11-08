use super::*;
use crate::tenancy::operational::{Hop, state::Denial};

fn charge(fixture: &Fixture, reference: &ResourceRef, status: Option<u16>) -> Result<()> {
    let state = fixture.service.current().map_err(failure)?;
    let profile = state
        .resource_admission
        .profile(reference)
        .context("profile")?;
    let ticket = fixture
        .service
        .operational
        .admit(&fixture.installation, reference, profile)
        .map_err(|e| anyhow::anyhow!("admission {e:?}"))?;
    let hop = Hop::new(
        Some(0),
        0,
        Some(reference.clone()),
        bitrouter_ai::tenancy::admission::FallbackRule::Stop,
        std::time::Instant::now(),
    );
    hop.admitted(ticket)
        .map_err(|e| anyhow::anyhow!("ticket {e:?}"))?;
    if let Some(status) = status {
        hop.headers(status, None);
    } else {
        hop.dropped();
    }
    Ok(())
}

#[tokio::test]
async fn real_charges_preserve_member_rpm_and_v1_local_v2_shared_health() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let mut a_rules = rules(2, Some(1), true);
    a_rules["observe_rate_group_id"] = json!("window");
    let a = provision(&fixture, "member-a", "a", None, a_rules).await?;
    let b = provision(&fixture, "member-b", "b", None, rules(2, Some(3), true)).await?;
    let local = provision(
        &fixture,
        "circuit",
        "local",
        None,
        rules(1, Some(100), true),
    )
    .await?;
    let mut bypass_rules = rules(2, None, false);
    bypass_rules["observe_rate_group_id"] = json!("window");
    let bypass = provision(&fixture, "bypass", "bypass", None, bypass_rules).await?;
    let input = request(
        &fixture,
        vec![a.clone(), b.clone(), local.clone(), bypass.clone()],
    )?;
    charge(&fixture, &a, None)?;
    let observed = read(&fixture, &input)?;
    ensure!(observed.profiles[0].availability == AvailabilityObservation::RateLimited);
    ensure!(observed.profiles[1].availability == AvailabilityObservation::Eligible);
    ensure!(observed.profiles[0].observed_shared_count == Some(1));
    ensure!(
        observed.profiles[3].rate.is_none()
            && observed.profiles[3].observed_shared_count == Some(1)
            && observed.profiles[3].health == HealthObservation::Disabled
    );
    charge(&fixture, &b, Some(401))?;
    let before = operational(&fixture)?;
    let db = durable(&fixture).await?;
    for _ in 0..3 {
        let observed = read(&fixture, &input)?;
        ensure!(
            observed.profiles[..2]
                .iter()
                .all(|entry| entry.health == HealthObservation::Blocked
                    && entry.availability == AvailabilityObservation::CircuitBlocked)
        );
        ensure!(observed.profiles[2].health == HealthObservation::Healthy);
        ensure!(
            observed.profiles[3].observed_shared_count == Some(2)
                && observed.profiles[3].availability == AvailabilityObservation::Eligible
        );
    }
    ensure!(before == operational(&fixture)? && db == durable(&fixture).await?);
    charge(&fixture, &bypass, Some(401))?;
    ensure!(read(&fixture, &input)?.profiles[3].observed_shared_count == Some(2));
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn independent_groups_and_installations_never_share_observations() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let other = keyed_fixture().await?;
    let first = provision(&fixture, "member", "first", None, rules(2, Some(3), true)).await?;
    let mut isolated_rules = rules(2, Some(3), true);
    isolated_rules["rate"]["rate_group_id"] = json!("other-window");
    isolated_rules["health_group_id"] = json!("other-health");
    let isolated = provision(&fixture, "isolated", "isolated", None, isolated_rules).await?;
    let other_pool = provision_in_pool(
        &fixture,
        "other-pool",
        "member",
        "other-pool",
        None,
        rules(2, Some(3), true),
    )
    .await?;
    let other_ref = provision(&other, "member", "other", None, rules(2, Some(3), true)).await?;
    charge(&fixture, &first, Some(401))?;
    let observed = read(
        &fixture,
        &request(&fixture, vec![first, isolated, other_pool])?,
    )?;
    ensure!(observed.profiles[0].health == HealthObservation::Blocked);
    ensure!(
        observed.profiles[1].health == HealthObservation::Healthy
            && observed.profiles[1]
                .rate
                .as_ref()
                .context("rate")?
                .shared_count
                == 0
    );
    ensure!(
        observed.profiles[2].health == HealthObservation::Healthy
            && observed.profiles[2]
                .rate
                .as_ref()
                .context("rate")?
                .shared_count
                == 0
    );
    let observed = read(&other, &request(&other, vec![other_ref])?)?;
    ensure!(
        observed.profiles[0].health == HealthObservation::Healthy
            && observed.profiles[0]
                .rate
                .as_ref()
                .context("rate")?
                .shared_count
                == 0
    );
    fixture.service.shutdown().await?;
    other.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn held_runtime_and_authority_locks_refuse_without_fencing_or_mutation() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, Some(3), true)).await?;
    let input = request(&fixture, vec![pin])?;
    let before = durable(&fixture).await?;
    let runtime = operational(&fixture)?;
    let refused = fixture.service.authority.with_write(|| {
        Ok(matches!(fixture.service.profile_observation_read(&fixture.operator,Some(&fixture.installation),Instant::now()+Duration::from_secs(5)),Err(error) if error.0.code==ErrorCode::CapacityExceeded))
    }).map_err(|code| anyhow::anyhow!("authority lock: {code:?}"))?;
    ensure!(refused);
    {
        let held = fixture
            .service
            .operational
            .snapshot()
            .map_err(|e| anyhow::anyhow!("runtime {e:?}"))?;
        let reader = fixture
            .service
            .profile_observation_read(
                &fixture.operator,
                Some(&fixture.installation),
                Instant::now() + Duration::from_secs(5),
            )
            .map_err(failure)?;
        ensure!(
            matches!(reader.sample(&input),Err(error) if error.0.code==ErrorCode::CapacityExceeded)
        );
        drop(held);
    }
    let permits = fixture.service.reads.acquire_many(8).await?;
    ensure!(
        matches!(fixture.service.profile_observation_read(&fixture.operator,Some(&fixture.installation),Instant::now()+Duration::from_secs(5)),Err(error) if error.0.code==ErrorCode::CapacityExceeded)
    );
    drop(permits);
    ensure!(
        matches!(fixture.service.profile_observation_read(&fixture.operator,Some(&fixture.installation),Instant::now()),Err(error) if error.0.code==ErrorCode::CapacityExceeded)
    );
    ensure!(read(&fixture, &input)?.profiles[0].health == HealthObservation::Healthy);
    ensure!(before == durable(&fixture).await? && runtime == operational(&fixture)?);
    fixture.service.shutdown().await?;
    ensure!(
        matches!(fixture.service.profile_observation_read(&fixture.operator,Some(&fixture.installation),Instant::now()+Duration::from_secs(5)),Err(error) if error.0.code==ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}

#[tokio::test]
async fn sampling_capacity_denial_preserves_fields_and_allocates_no_health_entries() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, None, true)).await?;
    let input = request(&fixture, vec![pin.clone()])?;
    let mut synthetic: bitrouter_ai::tenancy::admission_v2::AdmissionProfileV2 =
        serde_json::from_value(rules(2, None, true))?;
    for n in 0..crate::tenancy::operational::MAX_RUNTIME_KEYS {
        synthetic.health_group_id = Some(format!("occupied-{n}").try_into()?);
        drop(
            fixture
                .service
                .operational
                .admit(&fixture.installation, &pin, &synthetic)
                .map_err(|e| anyhow::anyhow!("fill {e:?}"))?,
        );
    }
    let before = operational(&fixture)?;
    let observed = read(&fixture, &input)?;
    ensure!(
        observed.profiles[0].availability == AvailabilityObservation::CapacityDenied
            && observed.profiles[0].health == HealthObservation::Healthy
            && observed.profiles[0].current_profile_revision.is_some()
    );
    ensure!(before == operational(&fixture)?);
    let state = fixture.service.current().map_err(failure)?;
    ensure!(matches!(
        fixture.service.operational.admit(
            &fixture.installation,
            &pin,
            state.resource_admission.profile(&pin).context("profile")?
        ),
        Err(Denial::Capacity)
    ));
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn original_deadline_is_not_reset_between_sampling_and_encoding() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, None, false)).await?;
    let input = request(&fixture, vec![pin])?;
    let reader = fixture
        .service
        .profile_observation_read(
            &fixture.operator,
            Some(&fixture.installation),
            Instant::now() + Duration::from_millis(250),
        )
        .map_err(failure)?;
    let sampled = reader.sample(&input).map_err(failure)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ensure!(
        matches!(reader.finish(&sampled),Err(error) if error.0.code==ErrorCode::CapacityExceeded)
    );
    drop(reader);
    ensure!(read(&fixture, &input)?.profiles[0].availability == AvailabilityObservation::Eligible);
    fixture.service.shutdown().await?;
    Ok(())
}
