//! Transparent x402 auto-paying proxy in front of BitRouter.
//!
//! An axum server on `127.0.0.1:4357` that sits between a caller (e.g. OpenCode)
//! and BitRouter's payment-gated `/v1/chat/completions` endpoint on
//! `127.0.0.1:4356`. The caller talks plain OpenAI-style HTTP and **never sees a
//! payment challenge**: for every request the proxy itself settles the charge
//! on Arc testnet (`eip155:5042002`) via the Proceeds x402 paywall using the
//! OWS `agent-treasury` wallet, then returns BitRouter's response as `200 OK`.
//!
//! Flow (proxy / payer side — all internal):
//!   1. Caller POSTs `/v1/chat/completions` (no payment headers).
//!   2. Proxy forwards to BitRouter. If BitRouter answers `402`, the proxy pays
//!      the Proceeds x402 paywall (EIP-3009 `transferWithAuthorization`, signed
//!      with `agent-treasury` via OWS) which settles USDC on-chain and yields a
//!      settlement tx hash.
//!   3. Proxy retries BitRouter with `X-Arc-Payment-Tx: 0x<txHash>`; BitRouter
//!      verifies the receipt on Arc and serves the inference.
//!   4. Proxy returns BitRouter's body to the caller as `200 OK`.
//!
//! Run with (BitRouter must already be serving + payment-gated on 4356):
//!   cargo run -p bitrouter-pay --example payment_proxy

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use bitrouter_pay::{AGENT_WALLET_ADDRESS, ARC_TESTNET_CHAIN_ID, ArcSigner, X402Client};
use serde_json::Value;

/// Address the proxy binds to (what the caller / OpenCode points at).
const PROXY_LISTEN: &str = "127.0.0.1:4357";
/// BitRouter inference endpoint the proxy pays for and forwards to.
const BITROUTER_UPSTREAM: &str = "http://127.0.0.1:4356/v1/chat/completions";
/// Proceeds x402 paywall that settles the USDC payment on Arc testnet.
const PROCEEDS_URL: &str = "https://myproceeds.xyz/api/x402/pay/cmqblj2m60004l704lp0jmr7u/infer";
/// Header BitRouter reads to verify the on-chain x402 settlement.
const ARC_PAYMENT_TX_HEADER: &str = "X-Arc-Payment-Tx";
/// OWS wallet that funds inference. Overridable via `OWS_WALLET_NAME`.
const WALLET_NAME: &str = "agent-treasury";
/// Price paid per request, in dollars (for logging).
const PRICE: &str = "0.001";

#[derive(Clone)]
struct ProxyState {
    /// x402 payer bound to the `agent-treasury` wallet (EIP-3009 via OWS).
    x402: Arc<X402Client>,
    /// Plain HTTP client used to talk to BitRouter directly.
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,bitrouter_pay=debug")),
        )
        .init();

    let signer = Arc::new(ArcSigner::new(WALLET_NAME.to_string())?);
    let payer = signer.address();

    let state = ProxyState {
        x402: Arc::new(X402Client::new(signer)),
        http: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    println!("x402 auto-paying proxy listening on http://{PROXY_LISTEN}");
    println!("  pays:     {PRICE} USDC per request (Arc testnet, eip155:{ARC_TESTNET_CHAIN_ID})");
    println!("  wallet:   {payer} (expected {AGENT_WALLET_ADDRESS})");
    println!("  paywall:  {PROCEEDS_URL}");
    println!("  upstream: {BITROUTER_UPSTREAM}");
    println!("  callers see plain 200 OK — payment is handled internally");

    let listener = tokio::net::TcpListener::bind(PROXY_LISTEN).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `POST /v1/chat/completions` — pay BitRouter via x402, then return its body.
///
/// The caller never sees the `402` challenge: payment is settled internally and
/// BitRouter is retried with proof of the on-chain settlement.
async fn chat_completions(State(state): State<ProxyState>, body: Bytes) -> Response {
    let json_body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response();
        }
    };

    // 1. Forward to BitRouter first. If it doesn't demand payment, pass through.
    let first = match state
        .http
        .post(BITROUTER_UPSTREAM)
        .json(&json_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[x402] BitRouter request failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("BitRouter request failed: {e}"),
            )
                .into_response();
        }
    };
    if first.status().as_u16() != 402 {
        return passthrough(first).await;
    }

    // 2. 402 — settle the charge on Arc via the Proceeds x402 paywall.
    let tx_hash = match pay_via_proceeds(&state.x402, &json_body).await {
        Some(tx) => tx,
        None => {
            eprintln!("[x402] payment did not yield a settlement tx hash");
            return (
                StatusCode::BAD_GATEWAY,
                "x402 payment did not yield a settlement tx hash",
            )
                .into_response();
        }
    };
    let tx_header = if tx_hash.starts_with("0x") || tx_hash.starts_with("0X") {
        tx_hash.clone()
    } else {
        format!("0x{tx_hash}")
    };
    println!("[x402] settled on Arc — txHash: {tx_header} — retrying BitRouter");

    // 3. Retry BitRouter with proof of the on-chain settlement.
    let paid = match state
        .http
        .post(BITROUTER_UPSTREAM)
        .header(ARC_PAYMENT_TX_HEADER, &tx_header)
        .json(&json_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[x402] BitRouter retry failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("BitRouter retry failed: {e}"),
            )
                .into_response();
        }
    };

    // 4. Return BitRouter's response to the caller as plain 200.
    let payload = paid.bytes().await.unwrap_or_default();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        payload,
    )
        .into_response()
}

/// Pay the Proceeds x402 paywall and return the settlement tx hash, if any.
///
/// Proceeds settles the USDC `transferWithAuthorization` on-chain and reports
/// the tx hash in the response body. It often returns `502` afterwards (its own
/// model backend timing out) even though the payment landed — the hash is still
/// present in the error body, so we scan both the success and failure text.
async fn pay_via_proceeds(x402: &X402Client, body: &Value) -> Option<String> {
    let text = match x402.post(PROCEEDS_URL, Some(body.clone())).await {
        Ok(v) => v.to_string(),
        Err(e) => e.to_string(),
    };
    extract_tx_hash(&text)
}

/// Extract the first `"txHash":"0x..."` value from arbitrary response text.
fn extract_tx_hash(text: &str) -> Option<String> {
    let needle = "\"txHash\":\"";
    let start = text.find(needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Relay a reqwest response back to the caller, preserving status and body.
async fn passthrough(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let payload = resp.bytes().await.unwrap_or_default();
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        payload,
    )
        .into_response()
}
