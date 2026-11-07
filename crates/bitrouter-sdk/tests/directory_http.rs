//! Public directory transport over real sockets, including adversarial responses.

#[path = "transport_protocol/server.rs"]
mod server;

use axum::{body::Body, http::Version, response::Response};
use bitrouter_ai::tenancy::{directory::DirectoryCursorV1, identity::InstallationId};
use bitrouter_sdk::{error::Error, tenancy::TenancyClient};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "synthetic-directory-operator-token";

fn page() -> Value {
    json!({"schema_version":1,"installation_id":"installation","authority_revision":7,"entries":[{"kind":"tenant","tenant_id":"tenant-b","revision_id":"revision","state":"active"}],"next_after":null})
}

#[tokio::test]
async fn directory_transport_encodes_cursor_and_rejects_invalid_requests_before_http() -> TestResult
{
    let server = server::serve(Version::HTTP_11, vec![Some(server::json(page())?)]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let installation: InstallationId = "installation".try_into()?;
    let cursor: DirectoryCursorV1 = "v1|tenant||tenant-a".try_into()?;
    for (revision, after, limit) in [(None, Some(&cursor), 1), (None, None, 0), (None, None, 101)] {
        assert!(matches!(
            client
                .directory(&installation, revision, after, limit)
                .await,
            Err(Error::Request(_))
        ));
    }
    assert!(server.records.lock().await.is_empty());
    let received = client
        .directory(&installation, Some(7.try_into()?), Some(&cursor), 2)
        .await?;
    assert_eq!(received.entries.len(), 1);
    let records = server.records.lock().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].connection, 1);
    assert_eq!(records[0].version, Version::HTTP_11);
    assert_eq!(records[0].method, "GET");
    assert_eq!(
        records[0].path,
        "/control/v1/tenancy/directory?limit=2&authority_revision=7&after=v1%7Ctenant%7C%7Ctenant-a"
    );
    assert_eq!(
        records[0].headers["authorization"],
        format!("Bearer {TOKEN}")
    );
    assert_eq!(
        records[0].headers["bitrouter-installation-id"],
        "installation"
    );
    assert!(records[0].body.is_empty());
    Ok(())
}

#[tokio::test]
async fn directory_transport_checks_installation_revision_order_cursor_and_count() -> TestResult {
    let mut wrong_installation = page();
    wrong_installation["installation_id"] = "foreign".into();
    let mut wrong_revision = page();
    wrong_revision["authority_revision"] = 8.into();
    let mut behind = page();
    behind["entries"][0]["tenant_id"] = "tenant-a".into();
    let mut duplicate = page();
    duplicate["entries"]
        .as_array_mut()
        .ok_or("entries")?
        .push(page()["entries"][0].clone());
    let mut wrong_cursor = page();
    wrong_cursor["next_after"] = "v1|tenant||tenant-z".into();
    let mut too_many = page();
    too_many["entries"].as_array_mut().ok_or("entries")?.push(
        json!({"kind":"tenant","tenant_id":"tenant-c","revision_id":"revision","state":"active"}),
    );
    let mut secret = page();
    secret["entries"][0]["secret"] = "sensitive-secret".into();
    let mut version = page();
    version["schema_version"] = 2.into();
    let cases = [
        wrong_installation,
        wrong_revision,
        behind,
        duplicate,
        wrong_cursor,
        too_many,
        secret,
        version,
    ];
    let replies = cases
        .into_iter()
        .map(|value| server::json(value).map(Some))
        .collect::<TestResult<Vec<_>>>()?;
    let server = server::serve(Version::HTTP_11, replies).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let cursor = "v1|tenant||tenant-a".try_into()?;
    for _ in 0..8 {
        let error = client
            .directory(
                &"installation".try_into()?,
                Some(7.try_into()?),
                Some(&cursor),
                1,
            )
            .await
            .err()
            .ok_or("invalid response accepted")?;
        assert!(!format!("{error:?} {error}").contains("sensitive-secret"));
    }
    assert_eq!(server.records.lock().await.len(), 8);
    Ok(())
}

#[tokio::test]
async fn directory_transport_caps_wire_bytes_and_preserves_conflict_without_retry() -> TestResult {
    let large = Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(format!("{}{}", page(), " ".repeat(1024 * 1024))))?;
    let conflict = Response::builder().status(409).header("content-type", "application/json").header("cache-control", "no-store")
        .body(Body::from(json!({"code":"revision_conflict","message":"observed revision does not match","dispatch_state":"not_started","operation_id":null,"execution_id":null}).to_string()))?;
    let server = server::serve(Version::HTTP_11, vec![Some(large), Some(conflict)]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let installation = "installation".try_into()?;
    let error = client
        .directory(&installation, None, None, 100)
        .await
        .err()
        .ok_or("oversize accepted")?;
    assert!(
        matches!(error, Error::Tenancy(ref failure) if failure.body_error.as_ref().is_some_and(|error| matches!(error, Error::Limit)))
    );
    let error = client
        .directory(&installation, Some(7.try_into()?), None, 100)
        .await
        .err()
        .ok_or("conflict accepted")?;
    assert!(
        matches!(error, Error::Tenancy(ref failure) if failure.status == 409 && failure.headers["cache-control"] == "no-store" && failure.body.as_ref().is_some_and(|body| body.code == bitrouter_ai::tenancy::operation::ErrorCode::RevisionConflict))
    );
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}
