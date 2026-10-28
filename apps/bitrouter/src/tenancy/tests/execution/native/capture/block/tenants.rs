//! Equal public names and the same real principal do not merge tenant ownership.
use super::*;

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn same_database_equal_names_keep_live_links_and_block_cohorts_tenant_scoped() -> Result<()> {
    let mut host = Host::pair().await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open().await?;
        let a_headers = host.endpoint_headers().await?;
        let (other, b_headers, _) = host.sibling.as_ref().context("actual second tenant")?;
        ensure!(
            host.fixture.installation == other.installation
                && host.fixture.tenant != other.tenant
                && host.fixture.principal == other.principal
                && host.fixture.binding == other.binding
        );
        let mut transplant = b_headers.clone();
        transplant.insert(
            crate::tenancy::execution::BLOCK_LINK_HEADER,
            a_headers
                .get(crate::tenancy::execution::BLOCK_LINK_HEADER)
                .context("actual A link")?
                .clone(),
        );
        host.rejected(transplant).await?;
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("provider")?
                .is_empty()
        );
        host.current()?
            .prompt(&native, "actual tenant A owned turn")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        host.current()?.checkpoint(&native).await?;
        let a_intents = host.records("owned_block_intent_v1").await?;
        let a_control = host.records("owned_block_control_v1").await?;
        ensure!(a_intents.len() == 1 && a_control.len() == 1);
        ensure!(
            fixture::records_for(other, "owned_block_intent_v1")
                .await?
                .is_empty()
        );
        let b_headers = b_headers.clone();
        host.close().await?;
        host.headers = Some(b_headers);
        let same_name = host.open().await?;
        ensure!(
            same_name == native,
            "fixture must reuse the same visible session name"
        );
        let b_endpoint = host.endpoint_headers().await?;
        ensure!(
            b_endpoint.get(crate::tenancy::execution::BLOCK_LINK_HEADER)
                != a_headers.get(crate::tenancy::execution::BLOCK_LINK_HEADER)
        );
        // A's still-valid ordinary key and immutable route cannot borrow B's
        // fresh capture even though all logical route/slot/controller names match.
        let mut reverse = a_headers;
        reverse.insert(
            crate::tenancy::execution::BLOCK_LINK_HEADER,
            b_endpoint
                .get(crate::tenancy::execution::BLOCK_LINK_HEADER)
                .context("actual B link")?
                .clone(),
        );
        host.rejected(reverse).await?;
        host.current()?
            .prompt(&same_name, "actual tenant B owned turn")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        host.current()?.checkpoint(&same_name).await?;
        let (other, _, _) = host.sibling.as_ref().context("second tenant")?;
        let b_intents = fixture::records_for(other, "owned_block_intent_v1").await?;
        let b_control = fixture::records_for(other, "owned_block_control_v1").await?;
        ensure!(b_intents.len() == 1 && b_control.len() == 1);
        ensure!(
            a_intents[0]["native"] == b_intents[0]["native"]
                && a_intents[0]["descriptor"]["principal"]
                    == b_intents[0]["descriptor"]["principal"]
                && a_intents[0]["descriptor"]["tenant"] != b_intents[0]["descriptor"]["tenant"]
                && a_intents[0]["session"] != b_intents[0]["session"]
        );
        ensure!(
            host.records("owned_block_intent_v1").await? == a_intents
                && host.records("owned_block_control_v1").await? == a_control,
            "foreign tenant changed the first cohort"
        );
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("actual sends")?
                .len()
                == 2
                && host.fixture.service.current().is_ok()
                && host.fixture.service.host_slots_for_test() == 0
        );
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    let checked = match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("cross-tenant Block observation unwound")),
    };
    host.finish(checked).await
}
