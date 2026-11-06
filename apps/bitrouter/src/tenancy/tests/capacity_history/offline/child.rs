//! Env isolation uses this already-built libtest; no new binary or runtime.
use super::*;

#[tokio::test]
#[ignore = "internal child of capacity_history restore qualification; requires an exact request"]
async fn restore_and_start() -> Result<()> {
    let input = std::env::var(REQUEST_ENV).context("restore child request missing")?;
    ensure!(input.len() <= 16 * 1024);
    let request: Request = serde_json::from_str(&input)?;
    let result = crate::offline::restore_with_options(
        &request.bundle,
        &request.digest,
        &request.database,
        &request.installation,
        KEY_ENV,
        32,
        options(&request.scratch),
    )
    .await;
    if request.reject {
        ensure!(result.is_err(), "invalid restore unexpectedly succeeded");
    } else {
        let restored = report(&result?)?;
        ensure!(
            restored["evidence_digest"] == request.digest
                && restored["evidence_anchor_checked"] == true
                && restored["database_inventory_verified"] == true
        );
        // A restore report is not readiness. Exercise the ordinary connector,
        // actual acquisition claim and configured key loader before any witness.
        let database = crate::db::connect_owned_tenancy(&format!(
            "sqlite://{}?mode=rw",
            request.database.display()
        ))
        .await?;
        let db = database.database().clone();
        let config = crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: Some(KEY_ENV.into()),
            ..Default::default()
        };
        let service = Service::open_owned_profile(database, &config, &Default::default()).await?;
        let checked = service.current().map_err(failure).and_then(|header| {
            ensure!(header.installation.as_str() == request.installation);
            Ok(())
        });
        let stopped = service.shutdown().await;
        let closed = db.close().await;
        checked?;
        stopped?;
        closed?;
        drop(service);
    }
    write_new(
        &request.report,
        &serde_json::to_vec(&serde_json::json!({
            "evidence_digest":request.digest,"installation":request.installation,
            "rejected":request.reject,"normal_startup":!request.reject
        }))?,
    )?;
    Ok(())
}
