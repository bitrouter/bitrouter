//! Version two binds the configuration layer without changing retained documents.
use super::{
    GenerationDefaultsEnvelopeV2, MAX_DEFAULTS_BYTES,
    envelope::{Sorted, validate_parts},
};
use crate::tenancy::{
    identity::Digest,
    validation::{Validate, ValidationError, bytes, require},
};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, io::Write};

impl Validate for GenerationDefaultsEnvelopeV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 2,
            "generation defaults envelope version",
        )?;
        validate_parts(self.canonical.as_ref(), &self.supplemental)?;
        bytes(self, MAX_DEFAULTS_BYTES)
    }
}

struct Normalized<'a>(&'a GenerationDefaultsEnvelopeV2);
impl Serialize for Normalized<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serializer.serialize_struct(
            "GenerationDefaultsEnvelopeV2",
            if self.0.canonical.is_some() { 4 } else { 3 },
        )?;
        object.serialize_field("schema_version", &self.0.schema_version)?;
        if let Some(canonical) = &self.0.canonical {
            object.serialize_field("canonical", canonical)?;
        }
        let values: BTreeMap<_, _> = self
            .0
            .supplemental
            .iter()
            .map(|(key, value)| (key, Sorted(value)))
            .collect();
        object.serialize_field("supplemental", &values)?;
        object.serialize_field("configuration_defaults", &self.0.configuration_defaults)?;
        object.end()
    }
}

impl GenerationDefaultsEnvelopeV2 {
    /// SHA-256 of `bitrouter.generation-defaults-envelope.v2\0` followed by
    /// compact JSON in schema_version, canonical (if present), supplemental
    /// then configuration_defaults order. The required layer choice always binds
    /// identity, even if both choices render the same body. Supplemental object keys are sorted recursively; arrays retain
    /// order and numbers use Serde JSON representation, not RFC 8785. The nested
    /// canonical encoding is unchanged. Suppressed defaults still bind identity.
    ///
    /// # Errors
    /// Rejects invalid, oversized or excessively nested defaults before hashing.
    pub fn digest(&self) -> Result<Digest, ValidationError> {
        self.validate()?;
        struct Sink(Sha256);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink(Sha256::new());
        sink.0
            .update(b"bitrouter.generation-defaults-envelope.v2\0");
        require(
            serde_json::to_writer(&mut sink, &Normalized(self)).is_ok(),
            "generation defaults envelope serialization",
        )?;
        sink.0
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .try_into()
    }
}
