//! Standalone reference: verify a NEAR AI Cloud model's TEE attestation (L1).
//!
//! Run it against the live provider:
//!
//! ```sh
//! export NEAR_BASE="https://cloud-api.near.ai/v1"          # or a bitrouter /v1/aci passthrough
//! export NEAR_KMS_ROOTS="3059...bcbc"                        # accepted dstack KMS root pubkey(s), comma-separated
//! export NEAR_IMAGE_DIGESTS="9b69...f677,c445...a698"        # and/or NEAR_WORKLOAD_IDS — at least one is required
//! export NVIDIA_EAT_KEY_PEM=/path/to/nvidia-nras-pub.pem     # NVIDIA's NRAS EAT verification key (EC public PEM)
//! cargo run -p bitrouter-attestation --example verify_near -- zai-org/GLM-5.1-FP8
//! ```
//!
//! The DCAP policy is REQUIRED to be pinned (KMS root + image/workload) — the
//! verifier refuses to run unpinned (spec §1.5 cond. 1). Without
//! `NVIDIA_EAT_KEY_PEM` the GPU NRAS check can't pass, so the verdict is
//! reported `unverified` — fail-closed, by design.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_attestation::{
    AciDcapVerifierPolicy, ConfidentialVerifier, DcapQuoteVerifier, NearVerifier, NvidiaEatKey,
    ReportTransport, ReqwestTransport,
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

    // NVIDIA's NRAS EAT verification key. Pinned in the daemon, not fetched
    // through the (untrusted) cloud. A wrong/absent key ⇒ GPU check fails closed.
    let nvidia_key = match std::env::var("NVIDIA_EAT_KEY_PEM") {
        Ok(path) => NvidiaEatKey::from_ec_pem(&std::fs::read(path)?)?,
        Err(_) => {
            eprintln!(
                "warning: NVIDIA_EAT_KEY_PEM unset — the GPU NRAS check cannot pass, so the \
                 verdict will be `unverified` (fail-closed)."
            );
            NvidiaEatKey::unconfigured()
        }
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
