#[cfg(feature = "swig")]
pub mod swig;

/// Build an x402-enabled [`reqwest_middleware::ClientWithMiddleware`] that wraps
/// requests with SWIG wallet x402 payment handling.
///
/// Available only when the `swig` feature is enabled.
#[cfg(feature = "swig")]
pub fn build_payment_client(
    swig_account: solana_pubkey::Pubkey,
    authority: solana_keypair::Keypair,
    role_id: u32,
    rpc_url: &str,
) -> reqwest_middleware::ClientWithMiddleware {
    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_owned());
    let signer = swig::SwigPaymentSigner::new(swig_account, authority, role_id, rpc);
    let x402_client = x402_signer::X402Client::new(signer);
    let middleware = x402_signer::middleware::X402PaymentMiddleware::new(x402_client);
    reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
        .with(middleware)
        .build()
}
