//! Original attribution and terminal facts survive retries and another reader.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::TerminalOutcome;
use bitrouter_ai::types::{Usage, UsageOrigin};

use super::{Fixture, accepted, row};
use crate::gateway::shared::engine::evidence::{Attribution, ObservedTerminal};
use crate::gateway::shared::store::{Kind, Store};
use crate::metering::{pricing::ModelPricing, tariff::FrozenTariff};

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let mut attribution = Attribution::fixture();
    attribution.provider_id = "original-provider".into();
    attribution.model_id = "original-model".into();
    attribution.tariff = Some(FrozenTariff {
        protocol: attribution.protocol.clone(),
        endpoint_profile: "fixture".into(),
        tariff_profile: Some("fixture".into()),
        pricing: Some(ModelPricing::cache_aware(
            Some(2.0),
            Some(0.5),
            Some(1.0),
            Some(3.0),
        )),
        pricing_version: "original-tariff".into(),
        billing_basis: "normalized-disjoint-usage-v1".into(),
        unavailable_reason: None,
    });
    let attempt = fixture
        .a
        .prepare_observed_attempt(&fixture.first.hash, &id, &fixture.stamp, &attribution)
        .await?;
    let prepared = row(second, Kind::Attempt, &attempt.attempt_id).await?;
    ensure!(prepared["attribution"] == serde_json::to_value(&attribution)?);
    ensure!(prepared["terminal_facts"].is_null());
    fixture
        .a
        .dispatch(&fixture.first.hash, &id, &attempt.attempt_id)
        .await?;
    let usage = Usage {
        prompt_tokens: 7,
        completion_tokens: 3,
        reasoning_tokens: 1,
        cache_read_tokens: 2,
        cache_write_tokens: 1,
        origin: UsageOrigin::ProviderReported,
        raw: Some(Box::new(
            serde_json::json!({"secret":"RAW_PROVIDER_SENTINEL"}),
        )),
        ..Usage::default()
    };
    let evidence = ObservedTerminal::capture(
        TerminalOutcome::Succeeded,
        Some(&usage),
        &attribution,
        Some(11),
    );
    ensure!(evidence.observation.usage.cost_micro_usd == Some(19));
    fixture
        .a
        .record_observed_terminal(&id, &attempt.attempt_id, &evidence)
        .await?;
    let original = row(second, Kind::Attempt, &attempt.attempt_id).await?;
    ensure!(original["terminal_facts"]["charge"]["charge_micro_usd"] == 19);
    ensure!(original["terminal_facts"]["observed_lifetime_ms"] == 11);
    ensure!(!serde_json::to_string(&original)?.contains("RAW_PROVIDER_SENTINEL"));
    fixture
        .a
        .record_observed_terminal(&id, &attempt.attempt_id, &evidence)
        .await?;
    ensure!(row(second, Kind::Attempt, &attempt.attempt_id).await? == original);
    // An exact replay includes the original measurements, not a new duration.
    let mut conflicting = evidence.clone();
    conflicting.facts.observed_lifetime_ms = Some(12);
    ensure!(
        fixture
            .a
            .record_observed_terminal(&id, &attempt.attempt_id, &conflicting)
            .await
            .is_err()
    );
    attribution
        .tariff
        .as_mut()
        .context("missing tariff")?
        .pricing = Some(ModelPricing::cache_aware(
        Some(20.0),
        Some(5.0),
        Some(10.0),
        Some(30.0),
    ));
    let repriced = ObservedTerminal::capture(
        TerminalOutcome::Succeeded,
        Some(&usage),
        &attribution,
        Some(11),
    );
    ensure!(
        fixture
            .a
            .record_observed_terminal(&id, &attempt.attempt_id, &repriced)
            .await
            .is_err()
    );
    ensure!(row(second, Kind::Attempt, &attempt.attempt_id).await? == original);
    ensure!(row(second, Kind::Resource, &fixture.pool).await?["active"] == 0);
    fixture.a.finish(&id, &TerminalOutcome::Succeeded).await?;
    let outbox = row(second, Kind::Outbox, &format!("gateway-terminal-{id}")).await?;
    ensure!(outbox["event"]["execution"]["state"]["result"]["usage"]["cost_micro_usd"] == 19);
    ensure!(!serde_json::to_string(&outbox)?.contains("original-provider"));
    let boundary = Attribution::boundary_fixture()?;
    let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let attempt = fixture
        .a
        .prepare_observed_attempt(&fixture.first.hash, &id, &fixture.stamp, &boundary)
        .await?;
    fixture
        .a
        .dispatch(&fixture.first.hash, &id, &attempt.attempt_id)
        .await?;
    let evidence = ObservedTerminal::capture(
        TerminalOutcome::Succeeded,
        Some(&usage),
        &boundary,
        Some(u64::MAX),
    );
    ensure!(serde_json::to_vec(&evidence.facts)?.len() > 64 * 1024);
    fixture
        .a
        .record_observed_terminal(&id, &attempt.attempt_id, &evidence)
        .await?;
    let stored = row(second, Kind::Attempt, &attempt.attempt_id).await?;
    ensure!(stored["terminal_facts"] == serde_json::to_value(&evidence.facts)?);
    ensure!(row(second, Kind::Resource, &fixture.pool).await?["active"] == 0);
    fixture.a.finish(&id, &TerminalOutcome::Succeeded).await?;
    Ok(())
}
