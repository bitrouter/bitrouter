//! Private, replayable attribution; public execution DTOs remain content-free.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{ExecutionUsage, TerminalObservation, TerminalOutcome};
use bitrouter_ai::types::{ApiProtocol, Usage, UsageOrigin};
use serde::{Deserialize, Serialize};

use crate::gateway::pipeline::types::RoutingTarget;
use crate::metering::pricing::{ChargeEvidence, PricingSource, unavailable_charge_evidence};
use crate::metering::tariff::FrozenTariff;

const FORMAT: u8 = 1;
const MAX_ATTRIBUTION_BYTES: usize = 64 * 1024;
// Terminal charge evidence repeats the admitted tariff plus its pricing version
// and optional unavailable reason. Those strings together fit within the original
// attribution bound; the remaining enum/numeric envelope fits in 4 KiB.
const MAX_TERMINAL_BYTES: usize = 2 * MAX_ATTRIBUTION_BYTES + 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::gateway::shared) struct Attribution {
    format: u8,
    pub provider_id: String,
    pub model_id: String,
    pub protocol: ApiProtocol,
    pub tariff: Option<FrozenTariff>,
}

impl Attribution {
    pub fn capture(target: &RoutingTarget, tariff: Option<FrozenTariff>) -> Result<Self> {
        let facts = Self {
            format: FORMAT,
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            protocol: target.api_protocol.clone(),
            tariff,
        };
        facts.validate()?;
        Ok(facts)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format == FORMAT,
            "unsupported dispatch attribution format"
        );
        for id in [&self.provider_id, &self.model_id] {
            ensure!(
                !id.is_empty() && id.len() <= 512,
                "invalid dispatch attribution identity"
            );
        }
        ensure!(
            self.tariff
                .as_ref()
                .is_none_or(|tariff| tariff.protocol == self.protocol),
            "dispatch tariff protocol mismatch"
        );
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_ATTRIBUTION_BYTES,
            "dispatch attribution exceeds byte limit"
        );
        // In particular, JSON must not silently turn nonfinite rates into null.
        ensure!(
            serde_json::from_slice::<Self>(&bytes)? == *self,
            "dispatch attribution is not lossless JSON"
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn boundary_fixture() -> Result<Self> {
        use crate::metering::pricing::{ContextTier, ModelPricing};
        let mut facts = Self::fixture();
        facts.provider_id = "p".into();
        facts.model_id = "m".into();
        facts.tariff = Some(FrozenTariff {
            protocol: facts.protocol.clone(),
            endpoint_profile: "fixture".into(),
            tariff_profile: Some("fixture".into()),
            pricing: Some(ModelPricing::cache_aware(
                Some(1.0),
                Some(1.0),
                Some(1.0),
                Some(1.0),
            )),
            pricing_version: format!("sha256:{}", "a".repeat(64)),
            billing_basis: "normalized-disjoint-usage-v1".into(),
            unavailable_reason: None,
        });
        for threshold in 0..1024 {
            facts
                .tariff
                .as_mut()
                .context("missing fixture tariff")?
                .pricing
                .as_mut()
                .context("missing fixture pricing")?
                .context_tiers
                .push(ContextTier {
                    above_input_tokens: threshold,
                    ..ContextTier::default()
                });
            if serde_json::to_vec(&facts)?.len() > MAX_ATTRIBUTION_BYTES {
                facts
                    .tariff
                    .as_mut()
                    .context("missing fixture tariff")?
                    .pricing
                    .as_mut()
                    .context("missing fixture pricing")?
                    .context_tiers
                    .pop();
                ensure!(serde_json::to_vec(&facts)?.len() > MAX_ATTRIBUTION_BYTES - 256);
                facts.validate()?;
                return Ok(facts);
            }
        }
        anyhow::bail!("fixture did not reach attribution boundary")
    }

    #[cfg(test)]
    pub fn fixture() -> Self {
        Self {
            format: FORMAT,
            provider_id: "fixture".into(),
            model_id: "fixture".into(),
            protocol: ApiProtocol::ChatCompletions,
            tariff: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::gateway::shared) struct TerminalFacts {
    format: u8,
    pub charge: ChargeEvidence,
    /// Monotonic time from attempt construction through the first observation.
    /// Includes reservation/dispatch authorization; this is not upstream latency.
    pub observed_lifetime_ms: Option<u64>,
}

#[derive(Clone)]
pub(in crate::gateway::shared) struct ObservedTerminal {
    pub observation: TerminalObservation,
    pub facts: TerminalFacts,
}

impl ObservedTerminal {
    pub fn capture(
        outcome: TerminalOutcome,
        usage: Option<&Usage>,
        attribution: &Attribution,
        observed_lifetime_ms: Option<u64>,
    ) -> Self {
        let usage = usage.filter(|usage| usage.origin != UsageOrigin::Unknown);
        // Only numeric measurements enter durable evidence. Provider raw JSON,
        // response content and diagnostics are deliberately never copied.
        let sanitized = usage.map_or_else(Usage::default, |usage| Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            origin: usage.origin,
            ..Usage::default()
        });
        let charge = match &attribution.tariff {
            Some(tariff) => tariff.charge_evidence(&sanitized, PricingSource::Configured),
            None => unavailable_charge_evidence(&sanitized, "admitted_tariff_unavailable"),
        };
        Self {
            observation: TerminalObservation {
                outcome,
                usage: ExecutionUsage {
                    origin: sanitized.origin,
                    prompt_tokens: usage.map(|usage| usage.prompt_tokens),
                    completion_tokens: usage.map(|usage| usage.completion_tokens),
                    reasoning_tokens: usage.map(|usage| usage.reasoning_tokens),
                    cache_read_tokens: usage.map(|usage| usage.cache_read_tokens),
                    cache_write_tokens: usage.map(|usage| usage.cache_write_tokens),
                    total_tokens: usage
                        .and_then(|usage| usage.prompt_tokens.checked_add(usage.completion_tokens)),
                    cost_micro_usd: charge
                        .charge_micro_usd
                        .and_then(|value| u64::try_from(value).ok()),
                },
                recorded_at_ms: 0, // Database clock replaces this on commit.
            },
            facts: TerminalFacts {
                format: FORMAT,
                charge,
                observed_lifetime_ms,
            },
        }
    }

    pub fn validate(&self, attribution: &Attribution) -> Result<()> {
        ensure!(
            self.facts.format == FORMAT,
            "unsupported terminal facts format"
        );
        attribution.validate()?;
        let evidence = &self.observation.usage;
        let usage = if evidence.origin == UsageOrigin::Unknown {
            None
        } else {
            Some(Usage {
                origin: evidence.origin,
                prompt_tokens: evidence
                    .prompt_tokens
                    .context("missing observed prompt tokens")?,
                completion_tokens: evidence
                    .completion_tokens
                    .context("missing observed completion tokens")?,
                reasoning_tokens: evidence
                    .reasoning_tokens
                    .context("missing observed reasoning tokens")?,
                cache_read_tokens: evidence
                    .cache_read_tokens
                    .context("missing observed cache read tokens")?,
                cache_write_tokens: evidence
                    .cache_write_tokens
                    .context("missing observed cache write tokens")?,
                ..Usage::default()
            })
        };
        let expected = Self::capture(
            self.observation.outcome.clone(),
            usage.as_ref(),
            attribution,
            self.facts.observed_lifetime_ms,
        );
        ensure!(
            expected.observation.usage == *evidence && expected.facts == self.facts,
            "terminal facts differ from original dispatch evidence"
        );
        ensure!(
            serde_json::to_vec(&self.facts)?.len() <= MAX_TERMINAL_BYTES,
            "terminal facts exceed byte limit"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_evidence_never_becomes_a_zero_charge() -> Result<()> {
        let attribution = Attribution::fixture();
        for usage in [
            None,
            Some(Usage {
                prompt_tokens: 9,
                origin: UsageOrigin::Unknown,
                ..Usage::default()
            }),
        ] {
            let evidence = ObservedTerminal::capture(
                TerminalOutcome::Succeeded,
                usage.as_ref(),
                &attribution,
                None,
            );
            evidence.validate(&attribution)?;
            assert_eq!(evidence.observation.usage.prompt_tokens, None);
            assert_eq!(evidence.observation.usage.cost_micro_usd, None);
            assert_eq!(evidence.facts.charge.charge_micro_usd, None);
            assert_eq!(evidence.facts.observed_lifetime_ms, None);
            let mut changed = evidence.clone();
            changed.observation.usage.prompt_tokens = Some(0);
            assert!(changed.validate(&attribution).is_err());
        }
        Ok(())
    }

    #[test]
    fn known_usage_without_price_remains_reconstructible() -> Result<()> {
        let attribution = Attribution::fixture();
        let usage = Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            cache_read_tokens: 2,
            origin: UsageOrigin::ProviderReported,
            raw: Some(Box::new(
                serde_json::json!({"credential":"PRIVATE_RAW_SENTINEL"}),
            )),
            ..Usage::default()
        };
        let evidence = ObservedTerminal::capture(
            TerminalOutcome::Succeeded,
            Some(&usage),
            &attribution,
            Some(0),
        );
        evidence.validate(&attribution)?;
        assert_eq!(evidence.observation.usage.total_tokens, Some(10));
        assert_eq!(evidence.observation.usage.cost_micro_usd, None);
        let json = serde_json::to_string(&evidence.facts)?;
        assert!(!json.contains("PRIVATE_RAW_SENTINEL"));
        assert_eq!(
            serde_json::from_str::<TerminalFacts>(&json)?,
            evidence.facts
        );
        let mut changed = evidence.clone();
        changed.facts.charge.charge_micro_usd = Some(0);
        assert!(changed.validate(&attribution).is_err());
        changed = evidence.clone();
        changed.facts.format = 2;
        assert!(changed.validate(&attribution).is_err());
        Ok(())
    }

    #[test]
    fn attribution_rejects_unknown_versions_and_oversized_identity() {
        let mut attribution = Attribution::fixture();
        attribution.format = 2;
        assert!(attribution.validate().is_err());
        attribution = Attribution::fixture();
        attribution.model_id = "x".repeat(513);
        assert!(attribution.validate().is_err());
    }
    #[test]
    fn admitted_boundary_tariff_always_fits_terminal_envelope() -> Result<()> {
        let attribution = Attribution::boundary_fixture()?;
        for usage in [
            None,
            Some(Usage {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                reasoning_tokens: u64::MAX,
                cache_read_tokens: u64::MAX,
                cache_write_tokens: u64::MAX,
                origin: UsageOrigin::ProviderReported,
                ..Usage::default()
            }),
        ] {
            let evidence = ObservedTerminal::capture(
                TerminalOutcome::Succeeded,
                usage.as_ref(),
                &attribution,
                Some(u64::MAX),
            );
            assert!(serde_json::to_vec(&evidence.facts)?.len() > MAX_ATTRIBUTION_BYTES);
            evidence.validate(&attribution)?;
        }
        Ok(())
    }
}
