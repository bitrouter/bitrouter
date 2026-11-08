use super::*;
use crate::{
    config::inference_routing::Policy,
    gateway::planning::{Signal, rank_manifest},
};

#[tokio::test]
async fn scheduling_operator_host_ranks_manifest_and_submits_explicit_v2() -> Result<()> {
    for stream in [false, true] {
        let harness = OrderedHarness::with_profiles(
            2,
            8,
            None,
            Value::Null,
            Some(&[profiled::rules(1000, false)?, profiled::rules(1000, false)?]),
        )
        .await?;
        harness.install_scheduler()?;
        let operator = harness.control(true, HttpVersion::Http1Only)?;
        let capabilities = operator.ordered_capabilities().await?;
        let manifest = operator
            .candidate_manifest(
                &harness.fixture.installation,
                &harness.fixture.tenant,
                &harness.revision.revision_id,
                &harness.route,
                GenerationProtocol::ChatCompletions,
            )
            .await?;
        let signals = [0, 1].map(|ordinal| Signal {
            ordinal,
            outstanding_work: 0,
            capacity: 8,
            bookable: true,
            affinity_millionths: if ordinal == 1 { 750_000 } else { 0 },
            latency: None,
        });
        let before = serde_json::to_value(&manifest)?;
        let binding = rank_manifest(
            &capabilities,
            &manifest,
            "planned-correlation".try_into()?,
            &signals,
            4096,
            Policy::PrefixAffinity,
        )?;
        ensure!(binding.selection.candidate_indices == [1, 0]);
        ensure!(
            binding.instance_id == capabilities.instance_id
                && binding.selection.route == manifest.route
                && binding.execution_revision == manifest.execution_revision
        );
        ensure!(serde_json::to_value(&manifest)? == before);
        let denied = harness
            .control(false, HttpVersion::Http1Only)?
            .candidate_manifest(
                &harness.fixture.installation,
                &harness.fixture.tenant,
                &harness.revision.revision_id,
                &harness.route,
                GenerationProtocol::ChatCompletions,
            )
            .await;
        ensure!(denied.is_err());
        let id = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                stream,
                HttpVersion::Http1Only,
            )
            .await?;
        ensure!(harness.observed()? == ["model-1", "model-0"]);
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.accepted_selection.candidate_indices == [1, 0]);
        let mut stale = manifest.clone();
        stale
            .runtime_observation
            .as_mut()
            .context("runtime observation")?
            .instance_id = "other-instance".try_into()?;
        ensure!(
            rank_manifest(
                &capabilities,
                &stale,
                "restart".try_into()?,
                &signals,
                4096,
                Policy::LoadAware
            )
            .is_err()
        );
        let mut stale = manifest.clone();
        stale.authority_revision = (manifest.authority_revision.get() + 1).try_into()?;
        ensure!(
            rank_manifest(
                &capabilities,
                &stale,
                "stale".try_into()?,
                &signals,
                4096,
                Policy::PrefixAffinity
            )
            .is_err()
        );
        let mut unknown = signals;
        unknown[0].ordinal = 2;
        ensure!(
            rank_manifest(
                &capabilities,
                &manifest,
                "unknown".try_into()?,
                &unknown,
                4096,
                Policy::LoadAware
            )
            .is_err()
        );
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn scheduling_ordered_skips_saturated_incompatible_target_before_queue() -> Result<()> {
    for stream in [false, true] {
        let harness =
            OrderedHarness::with_resource_edits(2, 8, None, Value::Null, None, |resources| {
                let route = resources
                    .iter_mut()
                    .find(|row| row["reference"]["resource_kind"] == "route")
                    .context("route")?;
                route["definition"]["routing"]["providers"]["provider-1"]["api_protocol"] =
                    json!([{"*":"messages"}]);
                Ok(())
            })
            .await?;
        let scheduler = harness.install_scheduler()?;
        let destination = &harness.scheduling_destinations[1];
        let config: crate::config::Config = serde_json::from_value(
            json!({"inherit_defaults":false,"registry":{"enabled":false},"providers":{
                "provider-1":{"api_base":destination.endpoint,"api_key":"holder-key","api_protocol":[{"*":"messages"}],"models":[{"id":"model-1"}]}
            }}),
        )?;
        let target = crate::config::routing_table::resolve_route_chain(
            &config,
            "provider-1:model-1",
            &Default::default(),
        )?
        .into_iter()
        .next()
        .context("target")?;
        let prompt: bitrouter_ai::types::Prompt =
            serde_json::from_value(json!({"model":"model-1","messages":[],"stream":false}))?;
        let (shape, work) = crate::gateway::scheduling::observation::Shape::from_prompt(&prompt)?;
        let holder = scheduler.book(
            scheduler.candidate(&target, 1)?,
            &target,
            work,
            shape,
            tokio::time::Instant::now() + Duration::from_secs(30),
        )?;
        let binding = harness
            .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
            .await?;
        let client =
            GatewayClient::with_options(&harness.endpoint, None, TransportOptions::default())?;
        let scoped = client.ordered_tenant(harness.token()?, &binding)?;
        let request: ChatRequest = serde_json::from_value(
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream,
            "response_format":{"type":"json_schema","json_schema":{"name":"result","strict":true,"schema":{"type":"object","properties":{},"additionalProperties":false}}}}),
        )?;
        let reply = scoped.chat_reply(&request).await?;
        ensure!(reply.metadata.status == 200);
        let id = reply.metadata.execution_id.context("id")?;
        if let Body::Stream(mut events) = reply.body {
            while let Some(item) = events.next().await {
                item?;
            }
        }
        let receipt = harness.receipt(&id).await?;
        ensure!(
            harness.observed()? == ["model-0"]
                && receipt.attempts.len() == 1
                && receipt.attempts[0].candidate_index == 0
        );
        ensure!(scheduler.inspect_queue().totals[0] == 1);
        drop(holder);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
