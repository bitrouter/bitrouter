//! Original GenerateContent counters, not canonical zero-filled Usage.
//! https://github.com/googleapis/googleapis/blob/master/google/ai/generativelanguage/v1beta/generative_service.proto
use super::{Counter, Evidence, Protocol};
use serde_json::Value;

impl Evidence {
    pub(super) fn observe_generate_content(&mut self, raw: &Value) {
        for (field, index) in [
            ("promptTokenCount", 0),
            ("candidatesTokenCount", 1),
            ("cachedContentTokenCount", 2),
            ("thoughtsTokenCount", 4),
        ] {
            self.field(index, raw.get(field), Protocol::GenerateContent);
        }
        self.known_keys(
            raw,
            &[
                "promptTokenCount",
                "candidatesTokenCount",
                "cachedContentTokenCount",
                "thoughtsTokenCount",
                "totalTokenCount",
            ],
        );
        if let Some(value) = raw.get("totalTokenCount") {
            self.total = match (number(value), self.total) {
                (Some(value), Counter::Absent) => Counter::Known(value),
                (Some(value), Counter::Known(previous)) if value >= previous => {
                    Counter::Known(value)
                }
                _ => Counter::Invalid,
            };
        }
        if let (Some(total), Some(input), Some(output), Some(thoughts)) = (
            self.total.value(),
            self.counters[0].value(),
            self.counters[1].value(),
            self.counters[4].value(),
        ) && input
            .checked_add(output)
            .and_then(|value| value.checked_add(thoughts))
            != Some(total)
        {
            self.total = Counter::Invalid;
        }
        if matches!(self.total, Counter::Invalid) {
            self.mark_incomplete();
        }
    }
}

/// Exact nonnegative int32, including integral decimal/exponent spellings.
/// Inspect only actual JSON number tokens; opaque native maps remain maps.
pub(super) fn number(value: &Value) -> Option<u64> {
    let raw = value.as_number()?.to_string();
    let negative = raw.starts_with('-');
    let raw = raw.strip_prefix('-').unwrap_or(&raw);
    let (mantissa, exponent) = raw.split_once(['e', 'E']).unwrap_or((raw, "0"));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut first = None;
    let mut last = 0;
    let mut length = 0;
    for (index, digit) in whole.bytes().chain(fraction.bytes()).enumerate() {
        if !digit.is_ascii_digit() {
            return None;
        }
        length = index + 1;
        if digit != b'0' {
            first.get_or_insert(index);
            last = index;
        }
    }
    let Some(first) = first else {
        return Some(0);
    };
    if negative {
        return None;
    }
    let scale = exponent
        .parse::<i64>()
        .ok()?
        .checked_sub(i64::try_from(fraction.len()).ok()?)?;
    let end = i64::try_from(length).ok()?.checked_add(scale)?;
    let digits = end.checked_sub(i64::try_from(first).ok()?)?;
    if !(1..=10).contains(&digits) || i64::try_from(last).ok()? >= end {
        return None;
    }
    let end = usize::try_from(end).ok()?;
    let mut value = 0_u64;
    for digit in whole
        .bytes()
        .chain(fraction.bytes())
        .skip(first)
        .take(end - first)
    {
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(digit - b'0'))?;
    }
    for _ in length..end {
        value = value.checked_mul(10)?;
    }
    (value <= i32::MAX as u64).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::receipts::evidence::Evidence;
    use serde_json::json;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn exact_native_int32_has_no_floating_point_or_object_coercion() -> TestResult {
        for (token, expected) in [
            ("0", Some(0)),
            ("-0.0", Some(0)),
            ("2.5e1", Some(25)),
            ("2147483647.0", Some(i32::MAX as u64)),
            ("2147483648", None),
            ("2147483646.9", None),
            ("-1", None),
            ("1e100", None),
            ("1e-100", None),
        ] {
            assert_eq!(number(&serde_json::from_str(token)?), expected, "{token}");
        }
        for value in [
            json!(null),
            json!("1"),
            json!({"$serde_json::private::Number":"1"}),
        ] {
            assert_eq!(number(&value), None);
        }
        Ok(())
    }

    #[test]
    fn native_projection_preserves_original_output_and_never_invents_optional_zero() {
        let mut observed = Evidence::default();
        observed.observe(
            Protocol::GenerateContent,
            &json!({"promptTokenCount":2,"candidatesTokenCount":3}),
        );
        let output = observed.project();
        assert_eq!(output.usage.input_tokens, Some(2));
        assert_eq!(output.usage.output_tokens, None);
        assert_eq!(output.usage.reasoning_tokens, None);
        assert!(!output.usage.complete);
        assert!(
            output
                .native_usage
                .iter()
                .any(|value| value.name == super::super::DIMENSIONS[1] && value.value == 3)
        );
        observed.observe(Protocol::GenerateContent, &json!({"promptTokenCount":2,"candidatesTokenCount":5,"thoughtsTokenCount":7,"totalTokenCount":14,"cachedContentTokenCount":0}));
        let output = observed.project();
        assert_eq!(output.usage.output_tokens, Some(12));
        assert_eq!(output.usage.reasoning_tokens, Some(7));
        assert_eq!(output.usage.cache_read_tokens, Some(0));
        assert_eq!(output.usage.cache_write_tokens, None);
        assert!(!output.usage.complete);
        observed.observe(
            Protocol::GenerateContent,
            &json!({"candidatesTokenCount":4}),
        );
        assert_eq!(observed.project().usage.output_tokens, None);
    }
}
