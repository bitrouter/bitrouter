//! End-to-end integration tests for `bitrouter-pay` against live services on
//! Arc testnet.
//!
//! Run with:
//!   cargo test -p bitrouter-pay --test integration_test -- --nocapture --include-ignored
//!
//! Prerequisites:
//!   - `.env` at repo root with `OWS_PASSPHRASE`, `CHAINLINK_ATTESTER_API_KEY`,
//!     and `OWS_VAULT_PATH`.
//!   - OWS wallet `agent-treasury` present at the vault path.
//!   - Arc testnet USDC funded in the wallet.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_pay::payment::x402::{
    TransferAuthorization, build_transfer_authorization_typed_data,
};
use bitrouter_pay::{ArcPaymentGate, ArcPaymentGateConfig, ArcSigner};
use bitrouter_sdk::{PaymentGate, PaymentRouteRequest};
use serde_json::json;

// ── Hardcoded test constants ──────────────────────────────────────────────────

const WALLET_NAME: &str = "agent-treasury";
const WALLET_ADDRESS: &str = "0xBB4CB05dA6ED0780cFDd0F088EaEEd420381DE38";
const PROCEEDS_URL: &str = "https://myproceeds.xyz/api/x402/pay/cmqblj2m60004l704lp0jmr7u/infer";
const PAY_TO: &str = "0xec56f2790840676a82ac11cbebb463eb28c9799a";
const AMOUNT_EXPECTED: u128 = 1000;
const CAIP2: &str = "eip155:5042002";
const CHAINLINK_API_KEY_FALLBACK: &str = "RLtYDAmBqQFXkxRpC6zhsQaVPA5qC4DC1gKNJVxn36qv";

// ── Environment helpers ───────────────────────────────────────────────────────

/// Load `.env` from repo root (cargo test CWD) and a few fallback paths.
fn load_env() {
    let _ = dotenvy::from_filename(".env");
    let _ = dotenvy::from_filename("../../.env");
    let _ = dotenvy::from_filename("plugins/bitrouter-pay/.env");
}

fn chainlink_api_key() -> String {
    std::env::var("CHAINLINK_ATTESTER_API_KEY")
        .unwrap_or_else(|_| CHAINLINK_API_KEY_FALLBACK.to_string())
}

// ── Test 1 — x402 payment loop ───────────────────────────────────────────────

/// Proves: raw HTTP + EIP-3009 signing against the live Proceeds x402 paywall.
///
/// Steps:
/// 1. POST to Proceeds URL, assert 402.
/// 2. Parse x402 v2 JSON challenge from body, select eip3009 accept entry.
/// 3. Build and sign `transferWithAuthorization` EIP-712 typed data.
/// 4. Retry POST with `X-Payment: <base64 proof>` header.
/// 5. Assert 200 with non-empty body.
#[tokio::test]
#[ignore]
async fn test_x402_payment_loop() {
    load_env();

    println!("=== Test 1: x402 payment loop ===\n");
    println!("Creating ArcSigner for wallet '{WALLET_NAME}'...");
    let signer = ArcSigner::new(WALLET_NAME.to_string()).unwrap_or_else(|e| {
        panic!(
            "ArcSigner::new failed: {e}\n\
             Ensure OWS_PASSPHRASE and OWS_VAULT_PATH are set correctly."
        )
    });

    let signer_addr = signer.address();
    println!("Signer address: {signer_addr}");
    assert_eq!(
        signer_addr.to_string().to_lowercase(),
        WALLET_ADDRESS.to_lowercase(),
        "wallet address mismatch — wrong wallet loaded?"
    );

    let http = reqwest::Client::new();
    let body = json!({ "prompt": "test" });

    // ── Step 1: initial request ───────────────────────────────────────────────
    println!("\n→ POST {PROCEEDS_URL}");
    let first = http
        .post(PROCEEDS_URL)
        .json(&body)
        .send()
        .await
        .expect("initial POST failed (network error)");

    let status = first.status();
    println!("← {status}");
    println!("Headers:");
    for (k, v) in first.headers() {
        println!("  {}: {}", k, v.to_str().unwrap_or("<binary>"));
    }
    assert_eq!(
        status.as_u16(),
        402,
        "expected 402 Payment Required from Proceeds"
    );

    // ── Step 2: parse challenge ───────────────────────────────────────────────
    let raw = first
        .text()
        .await
        .expect("failed to read 402 response body");
    println!("402 body:\n{raw}\n");

    let challenge: serde_json::Value =
        serde_json::from_str(&raw).expect("402 body is not valid JSON");

    let accepts = challenge["accepts"]
        .as_array()
        .expect("no 'accepts' array in x402 v2 challenge body");

    println!("Challenge has {} accept entries", accepts.len());

    let accept = accepts
        .iter()
        .find(|a| {
            a["scheme"].as_str() == Some("exact")
                && a["network"].as_str() == Some(CAIP2)
                && a["extra"]["assetTransferMethod"].as_str() == Some("eip3009")
        })
        .expect("no exact/eip3009 accept entry in x402 challenge");

    let pay_to: Address = accept["payTo"]
        .as_str()
        .expect("payTo is not a string")
        .parse()
        .expect("payTo is not a valid address");
    let amount: u128 = accept["amount"]
        .as_str()
        .expect("amount is not a string")
        .parse()
        .expect("amount is not a valid integer");
    let max_timeout = accept["maxTimeoutSeconds"].as_u64().unwrap_or(300);

    println!("Selected accept:");
    println!("  payTo:   {pay_to}");
    println!("  amount:  {amount} (USDC micro-units)");
    println!("  timeout: {max_timeout}s");

    // Sanity-check against hardcoded constants.
    assert_eq!(
        pay_to.to_string().to_lowercase(),
        PAY_TO.to_lowercase(),
        "payTo mismatch"
    );
    assert_eq!(amount, AMOUNT_EXPECTED, "amount mismatch");

    // ── Step 3: build EIP-3009 authorization ─────────────────────────────────
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock error")
        .as_secs();
    let valid_after = 0u64;
    let valid_before = now + max_timeout;
    let nonce = B256::from(rand::random::<[u8; 32]>());

    // EIP-712 domain comes from the challenge's `extra` field.
    let domain_name = accept["extra"]["name"].as_str().unwrap_or("USD Coin");
    let domain_version = accept["extra"]["version"].as_str().unwrap_or("2");

    println!("\nBuilding EIP-712 transferWithAuthorization...");
    println!("  domain:      {domain_name} v{domain_version}");
    println!("  from:        {signer_addr}");
    println!("  to:          {pay_to}");
    println!("  value:       {amount}");
    println!("  validAfter:  {valid_after}");
    println!("  validBefore: {valid_before}");
    println!("  nonce:       0x{}", hex::encode(nonce.0));

    let auth = TransferAuthorization {
        from: signer_addr,
        to: pay_to,
        value: amount,
        valid_after,
        valid_before,
        nonce,
    };

    let typed_data = build_transfer_authorization_typed_data(domain_name, domain_version, &auth);

    // ── Step 4: sign EIP-712 typed data via the OWS CLI ──────────────────────
    println!("\nSigning typed data with OWS CLI...");
    let sig = signer
        .sign_typed_data(&typed_data.to_string())
        .await
        .expect("OWS typed-data signing failed");

    // USDC EIP-3009 expects v = 27 or 28.
    let mut sig_bytes = Vec::with_capacity(65);
    sig_bytes.extend_from_slice(&sig.r().to_be_bytes::<32>());
    sig_bytes.extend_from_slice(&sig.s().to_be_bytes::<32>());
    sig_bytes.push(if sig.v() { 28 } else { 27 });
    let sig_hex = format!("0x{}", hex::encode(&sig_bytes));
    println!("Signature: {sig_hex}");

    // ── Step 5: build x402 v2 payment proof ──────────────────────────────────
    // Echo the challenge's `resource` and the full selected accept entry,
    // plus the signed authorization; base64url-encoded, no padding.
    let proof = json!({
        "x402Version": 2,
        "resource": challenge["resource"],
        "accepted": accept,
        "payload": {
            "signature": sig_hex,
            "authorization": {
                "from": signer_addr.to_string().to_lowercase(),
                "to": pay_to.to_string().to_lowercase(),
                "value": amount.to_string(),
                "validAfter": valid_after.to_string(),
                "validBefore": valid_before.to_string(),
                "nonce": format!("0x{}", hex::encode(nonce.0)),
            }
        }
    });
    let proof_b64 = URL_SAFE_NO_PAD.encode(proof.to_string());
    println!(
        "\nPAYMENT-SIGNATURE proof (base64url, first 80 chars): {}...",
        &proof_b64[..80.min(proof_b64.len())]
    );

    // ── Step 6: retry with payment ───────────────────────────────────────────
    println!("\n→ POST {PROCEEDS_URL}  (with PAYMENT-SIGNATURE header)");
    let paid = http
        .post(PROCEEDS_URL)
        .json(&body)
        .header("PAYMENT-SIGNATURE", &proof_b64)
        .send()
        .await
        .expect("payment POST failed (network error)");

    let paid_status = paid.status();
    println!("← {paid_status}");
    println!("Retry response headers:");
    for (k, v) in paid.headers() {
        println!("  {}: {}", k, v.to_str().unwrap_or("<binary>"));
    }
    let paid_body = paid.text().await.unwrap_or_default();
    println!("Response body:\n{paid_body}");

    assert!(
        paid_status.is_success(),
        "payment retry was rejected: {paid_status}\n{paid_body}"
    );
    assert!(!paid_body.is_empty(), "paid response body is empty");

    println!("\n✅ Test 1 passed — x402 EIP-3009 payment loop succeeded");
}

// ── Test 2 — Chainlink Confidential AI Attester ───────────────────────────────

/// Proves: inference submission, polling to completion, honest VerifiedExchange.
///
/// Steps:
/// 1. Run attested inference via the shared engine.
/// 2. Assert VerifiedExchange has a non-empty inference_id.
/// 3. Assert verified=false (unsigned dev-preview).
#[tokio::test]
#[ignore]
async fn test_chainlink_attester() {
    use bitrouter_attestation::{IntegrityProof, VerifiedExchange};
    use bitrouter_pay::run_attested_inference;

    load_env();

    println!("=== Test 2: Chainlink Confidential AI Attester ===\n");

    let code = "function add(a, b) { return a + b; }";

    println!("Submitting inference request...");
    println!("  model:    qwen3.6");
    println!("  resource: payload ({} bytes)", code.len());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let verified: VerifiedExchange = run_attested_inference(
        &chainlink_api_key(),
        "qwen3.6",
        r#"Review this code for bugs. Return JSON: {"pass": true, "issues": []}"#,
        code.as_bytes(),
        now,
    )
    .await
    .unwrap_or_else(|e| panic!("run_attested_inference failed: {e}"));

    println!("\nVerifiedExchange:");
    println!("  model:    {}", verified.model);
    println!("  verified: {}", verified.verified);

    assert!(!verified.verified, "unsigned digests must never verify");
    match verified.integrity {
        IntegrityProof::ChainlinkResourceDigests {
            inference_id,
            digests_consistent,
            ..
        } => {
            assert!(!inference_id.is_empty(), "inference_id is empty");
            println!("  inference_id:      {inference_id}");
            println!("  digests_consistent: {digests_consistent}");
        }
        other => panic!("expected ChainlinkResourceDigests, got {other:?}"),
    }

    println!("\n✅ Test 2 passed — Chainlink attested inference completed");
}

// ── Test 3 — Full gate flow ───────────────────────────────────────────────────

/// Proves: ArcPaymentGate composes x402 payment + Chainlink attestation in one
/// call and the ledger row is logged.
///
/// Steps:
/// 1. Create ArcPaymentGate with wallet_id + chainlink_api_key.
/// 2. Call gate.pay() with attested=true, model=qwen3.6.
/// 3. Assert the returned body deserializes as a VerifiedExchange.
/// 4. Assert it is honestly unverified (unsigned digests) with an inference id.
#[tokio::test]
#[ignore]
async fn test_full_gate_flow() {
    load_env();

    println!("=== Test 3: Full gate flow ===\n");

    println!("Creating ArcPaymentGate...");
    let gate = ArcPaymentGate::new(ArcPaymentGateConfig {
        wallet_id: WALLET_NAME.to_string(),
        chainlink_api_key: Some(chainlink_api_key()),
    })
    .unwrap_or_else(|e| panic!("ArcPaymentGate::new failed: {e}"));

    println!("Gate signer address: {}", gate.signer().address());

    let code = "function add(a, b) { return a + b; }";

    let request = PaymentRouteRequest {
        url: PROCEEDS_URL.to_string(),
        attested: true,
        body: Some(json!({ "prompt": "Review this code" })),
        mpp: false,
        model: Some("qwen3.6".to_string()),
        prompt: Some(format!("Review this code for bugs: {code}")),
    };

    println!("Calling gate.pay() (attested=true, model=qwen3.6)...");
    println!("This may take up to 10 minutes for Chainlink polling.");

    let result = gate
        .pay(request)
        .await
        .unwrap_or_else(|e| panic!("gate.pay() failed: {e}"));

    println!("\nGate result body:");
    println!(
        "{}",
        serde_json::to_string_pretty(&result.body).unwrap_or_default()
    );

    use bitrouter_attestation::{IntegrityProof, VerifiedExchange};
    let verified: VerifiedExchange =
        serde_json::from_value(result.body).expect("body is a VerifiedExchange");
    assert!(!verified.verified, "unsigned digests must never verify");
    match verified.integrity {
        IntegrityProof::ChainlinkResourceDigests {
            inference_id,
            digests_consistent,
            ..
        } => {
            assert!(!inference_id.is_empty(), "inference_id is empty");
            println!("digests_consistent: {digests_consistent}");
            println!("\nLedger entry: url={PROCEEDS_URL} ref={inference_id}",);
        }
        other => panic!("expected ChainlinkResourceDigests, got {other:?}"),
    }

    println!("\n✅ Test 3 passed — full gate flow (x402 + attestation) succeeded");
}
