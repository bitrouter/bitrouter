//! Exact JSON integer decoding, including integral decimal/exponent spellings.

use serde::{Deserialize, Deserializer, de::Error};
use serde_json::value::RawValue;

use super::identity::AuthorityRevision;

pub(super) fn integer<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    convert(raw.get())
        .ok_or_else(|| D::Error::custom("expected an exact bounded nonnegative integer"))
}

pub(super) fn nullable_integer<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<Box<RawValue>>::deserialize(deserializer)?;
    raw.map(|raw| {
        convert(raw.get())
            .ok_or_else(|| D::Error::custom("expected null or an exact bounded integer"))
    })
    .transpose()
}

fn convert<T: TryFrom<u64>>(raw: &str) -> Option<T> {
    T::try_from(exact_integer(raw)?).ok()
}

fn exact_integer(raw: &str) -> Option<u64> {
    // JSON Schema integer means a number with zero fractional part, not a
    // particular lexical spelling. Inspect raw tokens, never an f64 that may
    // already have rounded a fractional value into a privileged revision.
    // https://json-schema.org/draft/2020-12/json-schema-validation#name-type
    // https://www.rfc-editor.org/rfc/rfc8259#section-6
    let negative = raw.starts_with('-');
    let raw = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, exponent) = raw.split_once(['e', 'E']).unwrap_or((raw, "0"));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut first = None;
    let mut last = 0;
    for (index, digit) in whole.bytes().chain(fraction.bytes()).enumerate() {
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
    let exp_negative = exponent.starts_with('-');
    let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Beyond the input length plus 32 digits, exponent magnitude can only imply
    // an unrepresentable nonzero integer or a fractional value. Saturating this
    // parser avoids work proportional to the numeric exponent.
    let cap = i64::try_from(raw.len()).ok()?.checked_add(32)?;
    let magnitude = digits.bytes().fold(0_i64, |n, b| {
        n.saturating_mul(10)
            .saturating_add(i64::from(b - b'0'))
            .min(cap)
    });
    let exponent = if exp_negative { -magnitude } else { magnitude };
    let integer_digits = i64::try_from(whole.len())
        .ok()?
        .checked_add(exponent)?
        .checked_sub(i64::try_from(first).ok()?)?;
    let significant_digits = last - first + 1;
    if !(1..=16).contains(&integer_digits)
        || i64::try_from(significant_digits).ok()? > integer_digits
    {
        return None;
    }
    let mut value = 0_u64;
    for digit in whole
        .bytes()
        .chain(fraction.bytes())
        .skip(first)
        .take(significant_digits)
    {
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(digit - b'0'))?;
    }
    for _ in significant_digits..usize::try_from(integer_digits).ok()? {
        value = value.checked_mul(10)?;
    }
    (value <= AuthorityRevision::MAX).then_some(value)
}
