//! Socket-level protocol selection, including builds without the SDK http2 feature.

#[cfg(feature = "http2")]
#[path = "transport_protocol/http2.rs"]
mod http2;
#[path = "transport_protocol/server.rs"]
mod server;

use std::{error::Error as StdError, time::Duration};

use axum::http::Version;
use bitrouter_ai::gateway::operator::OperationLookup;
use bitrouter_sdk::{
    gateway::{GatewayClient, HttpVersion, TransportOptions},
    operator::{OperatorClient, OperatorOptions},
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "fixture-operator-token-0123456789abcdef";
const BUDGET: Duration = Duration::from_secs(5);

fn transport_options(http_version: HttpVersion) -> TransportOptions {
    TransportOptions {
        http_version,
        connect_timeout: BUDGET,
        read_timeout: Some(BUDGET),
        request_timeout: Some(BUDGET),
        ..Default::default()
    }
}

fn operator_options(http_version: HttpVersion) -> OperatorOptions {
    OperatorOptions {
        http_version,
        connect_timeout: BUDGET,
        request_timeout: BUDGET,
        ..Default::default()
    }
}

#[tokio::test]
async fn auto_and_http1_only_use_http1_on_cleartext_for_gateway_and_operator() -> TestResult {
    // This runs under --no-default-features as well as with reqwest/http2 unified
    // externally, without naming the feature-gated prior-knowledge variant.
    for version in [HttpVersion::Auto, HttpVersion::Http1Only] {
        let server = server::serve(
            Version::HTTP_11,
            vec![
                Some(server::json(json!({"status":"ok"}))?),
                Some(server::json(
                    json!({"status":"unresolved","operation_id":"op"}),
                )?),
            ],
        )
        .await?;
        let gateway = GatewayClient::with_options(
            &format!("{}/prefix/v1", server.endpoint),
            Some(TOKEN),
            transport_options(version),
        )?;
        assert_eq!(gateway.health().await?.status, "ok");
        let operator = OperatorClient::with_options(
            &format!("{}/prefix/operator/v1", server.endpoint),
            TOKEN,
            operator_options(version),
        )?;
        assert!(matches!(
            operator.operation("op").await?,
            OperationLookup::Unresolved { operation_id } if operation_id == "op"
        ));
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "/prefix/health");
        assert_eq!(
            records[1].path,
            "/prefix/operator/v1/operations?operation_id=op"
        );
        for record in records.iter() {
            assert_eq!(record.version, Version::HTTP_11);
            assert_eq!(record.method, "GET");
            assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
            assert!(record.body.is_empty());
        }
        // Independently constructed clients own distinct pools.
        assert_ne!(records[0].connection, records[1].connection);
    }
    Ok(())
}
