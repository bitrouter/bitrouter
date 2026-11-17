//! JOSE signing uses ring's P-256 implementation; keys never enter reports.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) fn generate() -> Result<String> {
    let key = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
        .map_err(|_| anyhow::anyhow!("could not generate Agent signing key"))?;
    Ok(B64.encode(key.as_ref()))
}
fn key(encoded: &str) -> Result<EcdsaKeyPair> {
    EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &B64.decode(encoded).context("invalid stored Agent key")?,
        &SystemRandom::new(),
    )
    .map_err(|_| anyhow::anyhow!("invalid stored Agent signing key"))
}
pub(super) fn public_jwk(encoded: &str) -> Result<Value> {
    let key = key(encoded)?;
    let bytes = key.public_key().as_ref();
    if bytes.len() != 65 || bytes[0] != 4 {
        bail!("invalid Agent public key");
    }
    Ok(
        json!({"kty":"EC","crv":"P-256","x":B64.encode(&bytes[1..33]),"y":B64.encode(&bytes[33..65])}),
    )
}
fn jwt(encoded: &str, header: Value, claims: Value) -> Result<String> {
    let input = format!(
        "{}.{}",
        B64.encode(serde_json::to_vec(&header)?),
        B64.encode(serde_json::to_vec(&claims)?)
    );
    let signature = key(encoded)?
        .sign(&SystemRandom::new(), input.as_bytes())
        .map_err(|_| anyhow::anyhow!("Agent proof signing failed"))?;
    Ok(format!("{input}.{}", B64.encode(signature.as_ref())))
}
pub(super) fn proof(
    encoded: &str,
    url: &str,
    method: &str,
    nonce: Option<&str>,
    token: Option<&str>,
) -> Result<String> {
    let mut claims = json!({"htu":url,"htm":method,"iat":chrono::Utc::now().timestamp(),"jti":uuid::Uuid::new_v4().to_string()});
    if let Some(nonce) = nonce {
        claims["nonce"] = nonce.into();
    }
    if let Some(token) = token {
        claims["ath"] = B64.encode(Sha256::digest(token.as_bytes())).into();
    }
    jwt(
        encoded,
        json!({"typ":"dpop+jwt","alg":"ES256","jwk":public_jwk(encoded)?}),
        claims,
    )
}
pub(super) fn assertion(encoded: &str, client: &str, url: &str) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    jwt(
        encoded,
        json!({"typ":"JWT","alg":"ES256","kid":"connection-key"}),
        json!({"iss":client,"sub":client,"aud":url,"iat":now,"exp":now+60,"jti":uuid::Uuid::new_v4().to_string()}),
    )
}
