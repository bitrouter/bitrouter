use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::digest::{Sha256Digest, canonical_sha256_digest};
use super::error::{PrimitiveError, Result};
use super::identity::{Ed25519Identity, Ed25519Signature, SigningKeyPair};
use super::jcs::canonical_json;
use super::types::TYPE_PROOF_ED25519_JCS;

pub const SIGNING_DOMAIN: &str = "bitrouter-signature-input/0\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedEnvelope {
    #[serde(rename = "type")]
    pub type_id: String,
    pub payload: Value,
    pub proofs: Vec<Ed25519JcsProof>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ed25519JcsProof {
    pub protected: ProofProtected,
    pub signature: Ed25519Signature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofProtected {
    #[serde(rename = "type")]
    pub type_id: String,
    pub payload_type: String,
    pub signer: Ed25519Identity,
    pub payload_hash: Sha256Digest,
}

#[derive(Serialize)]
struct SigningInput<'a> {
    #[serde(rename = "type")]
    type_id: &'a str,
    payload: &'a Value,
    protected: &'a ProofProtected,
}

impl SignedEnvelope {
    pub fn sign<T: Serialize>(
        type_id: impl Into<String>,
        payload: &T,
        signer: &SigningKeyPair,
    ) -> Result<Self> {
        let type_id = type_id.into();
        let payload = serde_json::to_value(payload)?;
        let protected = ProofProtected {
            type_id: TYPE_PROOF_ED25519_JCS.to_owned(),
            payload_type: type_id.clone(),
            signer: signer.identity(),
            payload_hash: canonical_sha256_digest(&payload)?,
        };
        let signature = signer.sign(&signing_input(&type_id, &payload, &protected)?);
        Ok(Self {
            type_id,
            payload,
            proofs: vec![Ed25519JcsProof {
                protected,
                signature,
            }],
        })
    }

    pub fn payload_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }

    pub fn verify_ed25519_jcs(
        &self,
        expected_type: &str,
        expected_signer: Option<&Ed25519Identity>,
    ) -> Result<()> {
        if self.type_id != expected_type {
            return Err(PrimitiveError::TypeMismatch {
                expected: expected_type.to_owned(),
                actual: self.type_id.clone(),
            });
        }
        if self.proofs.is_empty() {
            return Err(PrimitiveError::MissingProof);
        }
        for proof in &self.proofs {
            proof.verify(self, expected_signer)?;
        }
        Ok(())
    }
}

impl Ed25519JcsProof {
    pub fn verify(
        &self,
        envelope: &SignedEnvelope,
        expected_signer: Option<&Ed25519Identity>,
    ) -> Result<()> {
        if self.protected.type_id != TYPE_PROOF_ED25519_JCS {
            return Err(PrimitiveError::UnexpectedProofType(
                self.protected.type_id.clone(),
            ));
        }
        if self.protected.payload_type != envelope.type_id {
            return Err(PrimitiveError::PayloadTypeMismatch {
                proof: self.protected.payload_type.clone(),
                envelope: envelope.type_id.clone(),
            });
        }
        if let Some(expected) = expected_signer
            && &self.protected.signer != expected
        {
            return Err(PrimitiveError::UnexpectedSigner {
                expected: expected.to_string(),
                actual: self.protected.signer.to_string(),
            });
        }
        let expected_hash = canonical_sha256_digest(&envelope.payload)?;
        if self.protected.payload_hash != expected_hash {
            return Err(PrimitiveError::PayloadHashMismatch {
                expected: expected_hash.to_string(),
                actual: self.protected.payload_hash.to_string(),
            });
        }
        let input = signing_input(&envelope.type_id, &envelope.payload, &self.protected)?;
        self.protected.signer.verify(&input, &self.signature)
    }
}

pub fn signing_input(
    type_id: &str,
    payload: &Value,
    protected: &ProofProtected,
) -> Result<Vec<u8>> {
    let mut input = SIGNING_DOMAIN.as_bytes().to_vec();
    input.extend(canonical_json(&SigningInput {
        type_id,
        payload,
        protected,
    })?);
    Ok(input)
}

pub fn assert_no_inline_signature(payload: &Value) -> Result<()> {
    match payload {
        Value::Object(map) => {
            for key in ["signature", "sig", "order_sig"] {
                if map.contains_key(key) {
                    return Err(PrimitiveError::InlineSignature(key.to_owned()));
                }
            }
            for value in map.values() {
                assert_no_inline_signature(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                assert_no_inline_signature(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}
