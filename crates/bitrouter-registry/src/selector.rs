//! Collision-free provider/native routing identities.

use crate::document::{RegistryError, native_id};

/// An exact inventory lookup. The native component has been decoded once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSelector {
    /// Provider inventory namespace.
    pub provider: String,
    /// Opaque provider model ID, with original case and punctuation.
    pub model: String,
}

fn provider_name(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Encode all bytes except ASCII alphanumerics, dot, underscore and hyphen.
///
/// # Errors
/// Invalid provider namespaces and empty/control-bearing model IDs are rejected.
pub fn encode(provider: &str, model: &str) -> Result<String, RegistryError> {
    if !provider_name(provider) || !native_id(model) {
        return Err(RegistryError::invalid(
            "/model",
            "Invalid provider/native identity",
        ));
    }
    let mut result = format!("{provider}:~native/");
    for byte in model.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            result.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            result.push('%');
            result.push(char::from(HEX[usize::from(byte >> 4)]));
            result.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    Ok(result)
}

/// Recognize the reserved selector, rejecting malformed escapes and UTF-8.
/// Ordinary canonical/provider selectors return `None` for their usual resolver.
///
/// # Errors
/// Reserved selectors must carry one valid provider and one valid native ID.
pub fn decode(value: &str) -> Result<Option<NativeSelector>, RegistryError> {
    let Some((provider, encoded)) = value.split_once(":~native/") else {
        return Ok(None);
    };
    if !provider_name(provider) {
        return Err(RegistryError::invalid(
            "/model",
            "Invalid native selector provider",
        ));
    }
    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut position = 0;
    while let Some(byte) = bytes.get(position) {
        if *byte == b'%' {
            let pair = bytes.get(position + 1..position + 3).ok_or_else(|| {
                RegistryError::invalid("/model", "Truncated native selector escape")
            })?;
            let high = hex(pair[0]);
            let low = hex(pair[1]);
            let byte = high.zip(low).map(|(a, b)| (a << 4) | b).ok_or_else(|| {
                RegistryError::invalid("/model", "Invalid native selector escape")
            })?;
            decoded.push(byte);
            position += 3;
        } else if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            decoded.push(*byte);
            position += 1;
        } else {
            return Err(RegistryError::invalid(
                "/model",
                "Native selector contains an unescaped byte",
            ));
        }
    }
    let model = String::from_utf8(decoded)
        .map_err(|_| RegistryError::invalid("/model", "Native selector is not valid UTF-8"))?;
    if !native_id(&model) {
        return Err(RegistryError::invalid(
            "/model",
            "Native selector has an empty or control-bearing model ID",
        ));
    }
    Ok(Some(NativeSelector {
        provider: provider.into(),
        model,
    }))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_roundtrip_and_no_second_decode() -> Result<(), RegistryError> {
        for native in [
            "Vendor/Experimental:Beta%1",
            "%41",
            "厂商/模型:新",
            "with space",
        ] {
            let encoded = encode("example", native)?;
            assert_eq!(
                decode(&encoded)?,
                Some(NativeSelector {
                    provider: "example".into(),
                    model: native.into()
                })
            );
        }
        assert_eq!(
            encode("example", "Vendor/Experimental:Beta%1")?,
            "example:~native/Vendor%2FExperimental%3ABeta%251"
        );
        Ok(())
    }

    #[test]
    fn malformed_and_ambiguous_selectors_fail() -> Result<(), RegistryError> {
        for value in [
            "example:~native/%",
            "example:~native/%GG",
            "example:~native/%FF",
            "example:~native/%00",
            "example:~native/",
            "wrong/name:~native/a",
            "example:~native/a/b",
        ] {
            assert!(decode(value).is_err(), "{value}");
        }
        assert!(decode("example/model")?.is_none());
        assert!(decode("example:org/model")?.is_none());
        Ok(())
    }
}
