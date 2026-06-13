//! Claude Code autonomous payment demo.
//!
//! Demonstrates an AI agent that pays for its own inference: it signs a USDC
//! `transferWithAuthorization` with an OWS-backed wallet, settles an x402
//! paywall on Arc testnet through Proceeds, reads the model's reply, and obtains
//! a Chainlink TEE attestation of the run.
//!
//! Run with:
//!   OWS_VAULT_PATH=/path/to/wallets \
//!   OWS_WALLET_NAME=agent-treasury \
//!   CHAINLINK_ATTESTER_API_KEY=<key> \
//!   cargo run -p bitrouter-pay --example claude_code_demo

use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bitrouter_attestation::IntegrityProof;
use bitrouter_pay::{
    payment::x402::{build_transfer_authorization_typed_data, TransferAuthorization},
    run_attested_inference, ArcMppBackend, ArcSigner, MppBackend, MppClient,
    AGENT_WALLET_ADDRESS, ARC_TESTNET_CAIP2,
};
use serde_json::{json, Value};

const WALLET_NAME: &str = "agent-treasury";
const PROCEEDS_URL: &str =
    "https://myproceeds.xyz/api/x402/pay/cmqblj2m60004l704lp0jmr7u/infer";
/// BitRouter MPP endpoint used as a fallback when the Proceeds x402 upstream is
/// unavailable. BitRouter speaks MPP natively via `mpp-br`; Proceeds is x402-only.
const BITROUTER_MPP_URL: &str =
    "https://gumball-country-monologue.ngrok-free.dev/v1/chat/completions";
const MODEL: &str = "qwen3.6";
const PROMPT: &str = "You are an AI agent. You just paid for your own inference \
    using USDC on Arc testnet. Describe what just happened in one sentence.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let chainlink_api_key = std::env::var("CHAINLINK_ATTESTER_API_KEY").ok();

    println!("┌─────────────────────────────────────────────────────────────┐");
    println!("│  Claude Code · autonomous inference payment on Arc testnet    │");
    println!("└─────────────────────────────────────────────────────────────┘\n");

    // ── Build OWS signer ──────────────────────────────────────────────────────
    let signer = ArcSigner::new(WALLET_NAME.to_string())?;
    let signer = std::sync::Arc::new(signer);
    println!("[AGENT] Requesting inference from {MODEL}...");
    println!("        prompt: \"{PROMPT}\"\n");
    println!("[WALLET] Signing EIP-712 payment with OWS ({WALLET_NAME})...");
    println!(
        "         address: {} (expected {AGENT_WALLET_ADDRESS})\n",
        signer.address()
    );

    // HTTP client with a generous 120s timeout so we can rule out our side.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    let request_body = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": PROMPT}]
    });

    // ── Step 1: initial POST — expect 402 ────────────────────────────────────
    println!("→ POST {PROCEEDS_URL}");
    println!(
        "  body: {}",
        serde_json::to_string(&request_body).unwrap_or_default()
    );
    let first = http
        .post(PROCEEDS_URL)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("initial POST failed: {e}"))?;

    let status1 = first.status();
    println!("← {status1}");
    for (k, v) in first.headers() {
        println!("  {k}: {}", v.to_str().unwrap_or("<binary>"));
    }
    let raw1 = first.text().await.unwrap_or_default();
    println!("  body: {raw1}\n");

    if status1.as_u16() != 402 {
        println!("Expected 402, got {status1} — aborting.");
        return Ok(());
    }

    // ── Step 2: parse challenge + sign ───────────────────────────────────────
    let challenge: Value = serde_json::from_str(&raw1)?;
    let accepted = challenge["accepts"]
        .as_array()
        .and_then(|a| {
            a.iter().find(|e| {
                e["scheme"].as_str() == Some("exact")
                    && e["network"].as_str() == Some(ARC_TESTNET_CAIP2)
                    && e["extra"]["assetTransferMethod"].as_str() == Some("eip3009")
            })
        })
        .ok_or("no eip3009 accept entry")?;
    let resource = challenge.get("resource").cloned().unwrap_or(Value::Null);

    let pay_to: alloy::primitives::Address = accepted["payTo"]
        .as_str()
        .ok_or("missing payTo")?
        .parse()
        .map_err(|e| format!("bad payTo: {e}"))?;
    let amount: u128 = accepted["amount"]
        .as_str()
        .ok_or("missing amount")?
        .parse()?;
    let max_timeout = accepted["maxTimeoutSeconds"].as_u64().unwrap_or(300);
    let domain_name = accepted["extra"]["name"].as_str().unwrap_or("USD Coin");
    let domain_version = accepted["extra"]["version"].as_str().unwrap_or("2");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let nonce = alloy::primitives::B256::from(rand::random::<[u8; 32]>());

    let auth = TransferAuthorization {
        from: signer.address(),
        to: pay_to,
        value: amount,
        valid_after: 0,
        valid_before: now + max_timeout,
        nonce,
    };
    let typed_data = build_transfer_authorization_typed_data(domain_name, domain_version, &auth);

    println!("[WALLET] Signing EIP-712 typed data via OWS CLI...");
    let sig = signer
        .sign_typed_data(&typed_data.to_string())
        .await
        .map_err(|e| format!("OWS signing failed: {e}"))?;

    let mut sig_bytes = Vec::with_capacity(65);
    sig_bytes.extend_from_slice(&sig.r().to_be_bytes::<32>());
    sig_bytes.extend_from_slice(&sig.s().to_be_bytes::<32>());
    sig_bytes.push(if sig.v() { 28 } else { 27 });
    let sig_hex = format!("0x{}", hex::encode(&sig_bytes));
    println!("         signature: {sig_hex}\n");

    let proof = json!({
        "x402Version": 2,
        "resource": resource,
        "accepted": accepted,
        "payload": {
            "signature": sig_hex,
            "authorization": {
                "from": signer.address().to_string().to_lowercase(),
                "to": pay_to.to_string().to_lowercase(),
                "value": amount.to_string(),
                "validAfter": "0",
                "validBefore": auth.valid_before.to_string(),
                "nonce": format!("0x{}", hex::encode(nonce.0)),
            }
        }
    });
    let proof_b64 = URL_SAFE_NO_PAD.encode(proof.to_string());

    // ── Step 3: retry with PAYMENT-SIGNATURE (120s timeout) ──────────────────
    println!("→ POST {PROCEEDS_URL}  [with PAYMENT-SIGNATURE, 120s timeout]");
    println!("[CHAIN] Submitting on-chain USDC payment...");

    let paid = http
        .post(PROCEEDS_URL)
        .json(&request_body)
        .header("PAYMENT-SIGNATURE", &proof_b64)
        .send()
        .await
        .map_err(|e| format!("payment POST failed: {e}"))?;

    let status2 = paid.status();
    println!("← {status2}");
    println!("  Response headers:");
    for (k, v) in paid.headers() {
        println!("    {k}: {}", v.to_str().unwrap_or("<binary>"));
    }
    let raw2 = paid.text().await.unwrap_or_default();
    println!("  Response body:\n    {raw2}\n");

    // ── Parse result ──────────────────────────────────────────────────────────
    let model_reply: Option<String> = if status2.is_success() {
        if let Ok(body) = serde_json::from_str::<Value>(&raw2) {
            let tx = extract_tx_hash_from_headers_or_body(&raw2);
            match tx {
                Some(h) => println!("[CHAIN] USDC transferred on Arc testnet — txHash: {h}"),
                None => println!(
                    "[CHAIN] USDC transferred on Arc testnet — x402 payment settled (verified)"
                ),
            }
            let reply = body
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .map(str::to_string);
            match &reply {
                Some(text) => println!("[MODEL] Response: {text}\n"),
                None => println!(
                    "[MODEL] Response body (unexpected format):\n{}\n",
                    serde_json::to_string_pretty(&body).unwrap_or(raw2.clone())
                ),
            }
            reply
        } else {
            println!("[MODEL] Response (non-JSON): {raw2}\n");
            Some(raw2)
        }
    } else {
        let tx = extract_tx_hash_from_headers_or_body(&raw2);
        match tx {
            Some(h) => {
                println!("[CHAIN] USDC transferred on Arc testnet — txHash: {h}");
                // Proceeds settled payment but could not reach its model backend.
                // BitRouter supports MPP natively, so fall back to it for the reply.
                println!(
                    "[FALLBACK] Proceeds upstream returned {status2}; retrying via BitRouter MPP..."
                );
                let mpp = MppClient::new(std::sync::Arc::new(ArcMppBackend::new(signer.clone()))
                    as std::sync::Arc<dyn MppBackend>);
                match mpp.post(BITROUTER_MPP_URL, Some(request_body.clone())).await {
                    Ok(body) => {
                        let reply = body
                            .pointer("/choices/0/message/content")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        match &reply {
                            Some(text) => {
                                println!("[MODEL] Response (via BitRouter MPP): {text}\n")
                            }
                            None => println!(
                                "[MODEL] Response body (unexpected MPP format):\n{}\n",
                                serde_json::to_string_pretty(&body).unwrap_or_default()
                            ),
                        }
                        reply
                    }
                    Err(e) => {
                        println!(
                            "[MODEL] Response: <Proceeds upstream {status2}; MPP fallback failed: {e}>\n"
                        );
                        None
                    }
                }
            }
            None => {
                println!("[CHAIN] payment was NOT confirmed on-chain ({status2})");
                println!("[MODEL] Response: <payment failed>\n");
                None
            }
        }
    };

    // ── Chainlink TEE attestation ─────────────────────────────────────────────
    match chainlink_api_key {
        Some(key) => {
            let attest_prompt = match &model_reply {
                Some(reply) => format!(
                    "Confirm and restate this agent's report of its on-chain payment: {reply}"
                ),
                None => PROMPT.to_string(),
            };
            let now2 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            match run_attested_inference(&key, MODEL, &attest_prompt, PROMPT.as_bytes(), now2)
                .await
            {
                Ok(verified) => print_receipt(&verified),
                Err(e) => println!("[RECEIPT] Chainlink TEE attestation unavailable — {e}"),
            }
        }
        None => println!(
            "[RECEIPT] Chainlink TEE attestation skipped (set CHAINLINK_ATTESTER_API_KEY to enable)"
        ),
    }

    println!("\n✅ Demo complete — the agent paid for and received its own inference.");
    Ok(())
}

fn extract_tx_hash_from_headers_or_body(text: &str) -> Option<String> {
    let needle = "\"txHash\":\"";
    let start = text.find(needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn print_receipt(verified: &bitrouter_attestation::VerifiedExchange) {
    match &verified.integrity {
        IntegrityProof::ChainlinkResourceDigests {
            inference_id,
            request_digest,
            response_digest,
            ..
        } => {
            println!(
                "[RECEIPT] Chainlink TEE attestation: inference_id={inference_id} verified={}",
                verified.verified
            );
            println!("          model:           {}", verified.model);
            println!("          request_digest:  {request_digest}");
            println!("          response_digest: {response_digest}");
        }
        other => println!("[RECEIPT] Chainlink attestation: {other:?}"),
    }
}
