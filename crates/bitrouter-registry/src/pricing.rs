//! Exact proportional charges. Native codecs must prove counter semantics.

use std::collections::BTreeMap;
use std::str::FromStr;

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Zero};
use serde_json::Value;

use crate::document::{ReaderPolicy, RegistryError, integer};

/// Exact nonnegative decimal, bounded before allocating exponent-sized integers.
/// JSON numeric rates may use exponents; metered string amounts may not.
///
/// # Errors
/// Returns unavailable pricing for malformed, negative, or excessive numbers.
pub fn decimal(value: &str) -> Result<BigRational, RegistryError> {
    if value.is_empty() || value.len() > 128 {
        return Err(unavailable("Invalid or excessive decimal"));
    }
    let negative = value.starts_with('-');
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((base, exponent)) => (
            base,
            exponent
                .parse::<i32>()
                .map_err(|_| unavailable("Invalid decimal exponent"))?,
        ),
        None => (unsigned, 0),
    };
    if exponent.unsigned_abs() > 308 {
        return Err(unavailable("Decimal exponent exceeds supported bounds"));
    }
    let parts: Vec<_> = mantissa.split('.').collect();
    if parts.len() > 2
        || parts
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(unavailable("Expected a nonnegative decimal"));
    }
    let fraction = parts.get(1).map_or(0, |s| s.len()) as i32;
    let digits = parts.concat();
    let number = BigInt::from_str(&digits).map_err(|_| unavailable("Invalid decimal digits"))?;
    if negative && !number.is_zero() {
        return Err(unavailable("Expected a nonnegative decimal"));
    }
    let scale = fraction - exponent;
    if scale >= 0 {
        Ok(BigRational::new(number, BigInt::from(10).pow(scale as u32)))
    } else {
        Ok(BigRational::from_integer(
            number * BigInt::from(10).pow((-scale) as u32),
        ))
    }
}

/// Proven disjoint token counters. `None` means unknown; proven absence is zero.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    /// Input tokens outside either cache subset.
    pub uncached_input: Option<u64>,
    /// Input tokens read from cache, excluded from uncached input.
    pub cache_read: Option<u64>,
    /// Input tokens written to cache, excluded from other input categories.
    pub cache_write: Option<u64>,
    /// Output excluding the reasoning subset.
    pub non_reasoning_output: Option<u64>,
    /// Reasoning output excluded from non-reasoning output.
    pub reasoning_output: Option<u64>,
}

/// A codec-registered meter observation, already proven billable for this attempt.
#[derive(Debug, Clone)]
pub struct MeterUsage {
    /// Unit from the registered counter contract; must match the tariff.
    pub unit: String,
    /// Exact nonnegative quantity, including fractional units where documented.
    pub quantity: BigRational,
}

/// Price one proven billable attempt using only its selected interface's tariff.
/// `endpoint_profile` must be established by verified deployment configuration.
///
/// # Errors
/// Returns `registry.pricing_unavailable` if any rate, counter, meter, feature,
/// or processing basis is unknown. No partial or cross-interface charge is used.
pub fn charge(
    price: &Value,
    tokens: &TokenUsage,
    meters: &BTreeMap<String, MeterUsage>,
    endpoint_profile: Option<&str>,
    policy: &ReaderPolicy,
) -> Result<BigRational, RegistryError> {
    if !policy.supports(price) || price["currency"] != "USD" {
        return Err(unavailable(
            "Unsupported required price semantics or currency",
        ));
    }
    if let Some(declared) = price.get("endpoint_profile").and_then(Value::as_str)
        && (!matches!(declared, "openai_global" | "openai_us" | "openai_europe")
            || endpoint_profile != Some(declared))
    {
        return Err(unavailable(
            "Processing profile has not been verified for this deployment",
        ));
    }
    match price["kind"].as_str() {
        Some("token") => token_charge(price, tokens),
        Some("metered") => metered_charge(price, meters),
        _ => Err(unavailable("Unknown tariff kind")),
    }
}

fn token_charge(price: &Value, usage: &TokenUsage) -> Result<BigRational, RegistryError> {
    let input_total = [usage.uncached_input, usage.cache_read, usage.cache_write]
        .into_iter()
        .try_fold(0u64, |sum, count| sum.checked_add(count?))
        .ok_or_else(|| unavailable("Missing or overflowing input usage"))?;
    let tier = price
        .get("context_tiers")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers
                .iter()
                .rev()
                .find(|tier| integer(&tier["above_input_tokens"]).is_some_and(|n| input_total > n))
        });
    let mut total = BigRational::zero();
    for (group, name, count) in [
        ("input_tokens", "no_cache", usage.uncached_input),
        ("input_tokens", "cache_read", usage.cache_read),
        ("input_tokens", "cache_write", usage.cache_write),
        ("output_tokens", "text", usage.non_reasoning_output),
        ("output_tokens", "reasoning", usage.reasoning_output),
    ] {
        let count = count.ok_or_else(|| {
            unavailable("Unknown token category; absence must be proven by the codec")
        })?;
        if count == 0 {
            continue;
        }
        let rate = tier
            .and_then(|v| v.get(group))
            .and_then(|v| v.get(name))
            .or_else(|| price.get(group).and_then(|v| v.get(name)))
            .ok_or_else(|| unavailable("Missing rate for billable usage"))?;
        total += decimal(&rate.to_string())? * BigInt::from(count) / BigInt::from(1_000_000);
    }
    Ok(total)
}

fn metered_charge(
    price: &Value,
    usage: &BTreeMap<String, MeterUsage>,
) -> Result<BigRational, RegistryError> {
    let meters = price["meters"]
        .as_array()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| unavailable("Missing meters"))?;
    let mut total = BigRational::zero();
    for meter in meters {
        let id = meter["id"]
            .as_str()
            .ok_or_else(|| unavailable("Invalid meter identity"))?;
        let counter = usage
            .get(id)
            .ok_or_else(|| unavailable("Meter counter is not implemented or usage is unknown"))?;
        if counter.quantity < BigRational::zero()
            || meter["unit"].as_str() != Some(counter.unit.as_str())
        {
            return Err(unavailable(
                "Meter counter has incompatible units or negative quantity",
            ));
        }
        let quantity = integer(&meter["quantity"])
            .filter(|n| *n > 0)
            .ok_or_else(|| unavailable("Invalid tariff quantity"))?;
        let amount = meter["amount"]
            .as_str()
            .ok_or_else(|| unavailable("Invalid tariff amount"))?;
        total += &counter.quantity * decimal(amount)? / BigInt::from(quantity);
    }
    Ok(total)
}

/// Aggregate billable attempts without per-request or per-meter rounding.
///
/// # Errors
/// Any unknown billed retry/partial attempt makes the logical total unavailable.
pub fn sum_attempts(
    attempts: impl IntoIterator<Item = Option<BigRational>>,
) -> Result<BigRational, RegistryError> {
    attempts.into_iter().try_fold(
        BigRational::new(BigInt::zero(), BigInt::one()),
        |total, amount| {
            amount
                .map(|n| total + n)
                .ok_or_else(|| unavailable("A billable attempt has unknown usage"))
        },
    )
}

fn unavailable(message: &str) -> RegistryError {
    RegistryError { code:"registry.pricing_unavailable", path:"/pricing".into(), message:message.into(), recovery:"Provide verified applicable rates and codec counters, or upgrade to software supporting this tariff".into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decisions_are_proportional_and_unknown_attempts_never_become_free()
    -> Result<(), RegistryError> {
        let price = json!({"kind":"metered","requires":["pricing.metered.v1"],"currency":"USD","meters":[{"id":"answered_question","unit":"decision","quantity":1000,"amount":"0.20"}]});
        let usage = [(
            "answered_question".into(),
            MeterUsage {
                unit: "decision".into(),
                quantity: decimal("3")?,
            },
        )]
        .into();
        let amount = charge(
            &price,
            &TokenUsage::default(),
            &usage,
            None,
            &ReaderPolicy::default(),
        )?;
        assert_eq!(amount, decimal("0.0006")?);
        assert_eq!(
            sum_attempts([Some(amount.clone()), Some(amount.clone())])?,
            decimal("0.0012")?
        );
        assert!(sum_attempts([Some(amount), None]).is_err());
        assert!(
            charge(
                &price,
                &TokenUsage::default(),
                &BTreeMap::new(),
                None,
                &ReaderPolicy::default()
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn cached_and_reasoning_subsets_are_disjoint_and_tiers_inherit_only_base()
    -> Result<(), RegistryError> {
        let price = json!({"kind":"token","currency":"USD","input_tokens":{"no_cache":2,"cache_read":1},"output_tokens":{"text":4,"reasoning":8},"context_tiers":[{"above_input_tokens":100,"input_tokens":{"no_cache":3}}]});
        let usage = TokenUsage {
            uncached_input: Some(100),
            cache_read: Some(20),
            cache_write: Some(0),
            non_reasoning_output: Some(5),
            reasoning_output: Some(5),
        };
        assert_eq!(
            charge(
                &price,
                &usage,
                &BTreeMap::new(),
                None,
                &ReaderPolicy::default()
            )?,
            decimal("0.00038")?
        );
        let unknown = TokenUsage {
            reasoning_output: None,
            ..usage
        };
        assert!(
            charge(
                &price,
                &unknown,
                &BTreeMap::new(),
                None,
                &ReaderPolicy::default()
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn high_precision_and_resource_bounds_are_explicit() -> Result<(), RegistryError> {
        assert_eq!(decimal("-0.0")?, BigRational::zero());
        assert_eq!(
            decimal("0.100000000000000000000001")? * BigInt::from(10),
            decimal("1.00000000000000000000001")?
        );
        assert!(decimal("1e999999999").is_err());
        assert!(decimal("-1").is_err());
        Ok(())
    }
}
