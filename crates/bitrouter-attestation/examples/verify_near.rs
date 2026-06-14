//! Standalone reference: verify a NEAR AI Cloud model's TEE attestation (L1).
//!
//! Run it against the live provider:
//!
//! ```sh
//! export NEAR_BASE="https://cloud-api.near.ai/v1"          # or a bitrouter /v1/aci passthrough
//! export NEAR_KMS_ROOTS="3059...bcbc"                        # accepted dstack KMS root pubkey(s), comma-separated
//! export NEAR_IMAGE_DIGESTS="9b69...f677,c445...a698"        # and/or NEAR_WORKLOAD_IDS — at least one is required
//! # NVIDIA's NRAS key is fetched from its JWKS automatically; set
//! # NVIDIA_EAT_KEY_PEM=/path/to/nvidia-nras-pub.pem to pin a single key instead.
//! cargo run -p bitrouter-attestation --example verify_near -- zai-org/GLM-5.1-FP8
//! ```
//!
//! The DCAP policy is REQUIRED to be pinned (KMS root + image/workload) — the
//! verifier refuses to run unpinned (spec §1.5 cond. 1).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_attestation::{
    AciDcapVerifierPolicy, ConfidentialVerifier, DcapQuoteVerifier, NVIDIA_NRAS_JWKS_URL,
    NearVerifier, NvidiaEatKey, ReportTransport, ReqwestTransport,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

fn env_list(key: &str) -> Vec<String> {
    std::env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let model = std::env::args()
        .nth(1)
        .ok_or("usage: verify_near <model> (e.g. zai-org/GLM-5.1-FP8)")?;

    let base = std::env::var("NEAR_BASE").unwrap_or_else(|_| "https://cloud-api.near.ai/v1".into());

    // The load-bearing pin (spec §1.5). Construction fails if unpinned.
    let policy = AciDcapVerifierPolicy::new(
        env_list("NEAR_WORKLOAD_IDS"),
        env_list("NEAR_IMAGE_DIGESTS"),
        env_list("NEAR_KMS_ROOTS"),
    )?;

    // NVIDIA's NRAS EAT key. By default we fetch NVIDIA's JWKS (its signing keys
    // rotate, so the right one is chosen per request by the EAT `kid`); set
    // NVIDIA_EAT_KEY_PEM to pin a single key instead. Resolved in this trusted
    // process, never through the (untrusted) cloud.
    let nvidia_key = match std::env::var("NVIDIA_EAT_KEY_PEM") {
        Ok(path) => NvidiaEatKey::from_ec_pem(&std::fs::read(path)?)?,
        Err(_) => NvidiaEatKey::fetch_jwks(NVIDIA_NRAS_JWKS_URL).await?,
    };

    let transport = Arc::new(ReqwestTransport::new(&base)) as Arc<dyn ReportTransport>;
    let verifier = NearVerifier::new(
        transport,
        Arc::new(DcapQuoteVerifier::default()),
        Arc::new(policy),
        Arc::new(nvidia_key),
    );

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let verdict = verifier.attestation_cached(&model, now).await?;

    println!("{}", serde_json::to_string_pretty(&verdict)?);
    if verdict.verified {
        println!("\n✅ {model} is a genuine, policy-pinned TEE.");
    } else {
        println!("\n❌ {model} did NOT verify — see the per-check breakdown above.");
    }
    Ok(())
}
