//! A later real completion is found through protected HTTP pagination.
use super::*;
use std::collections::BTreeSet;

async fn page(
    harness: &Harness,
    client: &reqwest::Client,
    after: Option<&ExecutionId>,
) -> Result<ExecutionPage> {
    let mut request = client
        .get(format!(
            "{}/control/v1/tenants/{}/executions",
            harness.base, harness.fixture.tenant
        ))
        .bearer_auth(
            harness
                .credential
                .secret
                .as_deref()
                .context("receipt token")?,
        )
        .header(
            "bitrouter-installation-id",
            harness.fixture.installation.as_str(),
        )
        .query(&[("correlation_id", "same-correlation"), ("limit", "1")]);
    if let Some(after) = after {
        request = request.query(&[("after", after.as_str())]);
    }
    let response = request.send().await?;
    ensure!(response.status() == StatusCode::OK);
    Ok(response.json().await?)
}

#[tokio::test]
async fn same_correlation_later_completion_is_found_by_public_rescan() -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let checked = async {
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let first_id = execution_id(&response)?;
        let _ = response.bytes().await?;
        let first = page(&harness, &client, None).await?;
        ensure!(
            first.execution_ids == vec![first_id.clone()]
                && !first.multiple_executions
                && first.next_after.is_none()
        );
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);

        // This second actual send completes only after the first public scan.
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let second_id = execution_id(&response)?;
        let _ = response.bytes().await?;
        ensure!(first_id != second_id && harness.calls.load(Ordering::SeqCst) == 2);
        // IDs are not completion ordered: restart the scan, do not assume its
        // former terminal cursor can discover a lexically earlier new ID.
        let first_page = page(&harness, &client, None).await?;
        ensure!(first_page.execution_ids.len() == 1 && first_page.multiple_executions);
        let cursor = first_page
            .next_after
            .as_ref()
            .context("second page cursor")?;
        let second_page = page(&harness, &client, Some(cursor)).await?;
        ensure!(
            second_page.execution_ids.len() == 1
                && second_page.multiple_executions
                && second_page.next_after.is_none()
        );
        let observed = first_page
            .execution_ids
            .into_iter()
            .chain(second_page.execution_ids)
            .collect::<BTreeSet<_>>();
        ensure!(observed == BTreeSet::from([first_id.clone(), second_id.clone()]));
        let empty = page(
            &harness,
            &client,
            Some(observed.last().context("last execution")?),
        )
        .await?;
        ensure!(empty.execution_ids.is_empty() && empty.multiple_executions);
        for id in [first_id, second_id] {
            ensure!(harness.receipt(&client, &id).await?.execution_id == id);
        }
        ensure!(
            harness.calls.load(Ordering::SeqCst) == 2,
            "receipt reads initiated inference"
        );
        Ok(())
    }
    .await;
    let shutdown = harness.fixture.service.shutdown().await;
    checked?;
    shutdown?;
    Ok(())
}
