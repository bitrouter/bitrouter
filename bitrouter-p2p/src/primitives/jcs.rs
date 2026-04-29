use super::error::{PrimitiveError, Result};

pub fn canonical_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let json = serde_jcs::to_string(value)
        .map_err(|err| PrimitiveError::JsonCanonicalization(err.to_string()))?;
    Ok(json.into_bytes())
}
