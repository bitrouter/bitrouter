//! Real provider usage -> strict035 -> effective retained tariff -> Block fact.
use super::*;
use fixture::Options;

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_cached_usage_requires_the_effective_retained_tariff() -> Result<()> {
    let cases = [
        (
            "missing-read",
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0,
            "cache_write_micro_usd_per_token":7.0}),
            None,
        ),
        (
            "missing-write",
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0,
            "cache_read_micro_usd_per_token":5.0}),
            None,
        ),
        (
            "explicit-zero",
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0,
            "cache_read_micro_usd_per_token":0.0,"cache_write_micro_usd_per_token":0.0}),
            Some(12),
        ),
        (
            "explicit-positive",
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0,
            "cache_read_micro_usd_per_token":5.0,"cache_write_micro_usd_per_token":7.0}),
            Some(43),
        ),
        (
            "tier-inherits-base",
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0,
            "cache_read_micro_usd_per_token":5.0,"cache_write_micro_usd_per_token":7.0,
            "context_tiers":[{"above_input_tokens":4,"input_micro_usd_per_token":10.0,"output_micro_usd_per_token":11.0}]}),
            Some(83),
        ),
        (
            "tier-does-not-inherit-prior-tier",
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0,
            "context_tiers":[{"above_input_tokens":2,"cache_read_micro_usd_per_token":5.0,"cache_write_micro_usd_per_token":7.0},
                {"above_input_tokens":4,"input_micro_usd_per_token":10.0,"output_micro_usd_per_token":11.0}]}),
            None,
        ),
    ];
    for (name, tariff, expected) in cases {
        let mut host = Host::configured(Options {
            pricing: Some([tariff.clone(), tariff]),
            usage: Some(
                json!({"prompt_tokens":8,"completion_tokens":2,"total_tokens":10,
                "prompt_tokens_details":{"cached_tokens":2,"cache_write_tokens":3},
                "completion_tokens_details":{"reasoning_tokens":0}}),
            ),
            ..Default::default()
        })
        .await?;
        let checked = std::panic::AssertUnwindSafe(async {
            let native = host.open().await?;
            let current = host.current()?;
            current
                .prompt(&native, "Reply with exactly native reply; no tools")
                .await?;
            tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
            current
                .checkpoint(&native)
                .await?
                .context("real frozen source")?;
            let view = current.review_checkpoint(&native, None).await?;
            current
                .submit_assessment(&native, label(&view, name)?)
                .await?;
            current.checkpoint(&native).await?;
            let intents = host.records("owned_block_intent_v1").await?;
            ensure!(intents.len() == 1);
            let id = intents[0]["intent"]["request_id"]
                .as_str()
                .context("actual intent")?
                .try_into()?;
            let receipt = crate::tenancy::receipts::store::lookup(
                &host.fixture.db,
                &host.fixture.installation,
                &host.fixture.tenant,
                &id,
            )
            .await?
            .context("actual035")?;
            ensure!(
                receipt.usage.complete
                    && receipt.usage.input_tokens == Some(8)
                    && receipt.usage.output_tokens == Some(2)
                    && receipt.usage.cache_read_tokens == Some(2)
                    && receipt.usage.cache_write_tokens == Some(3)
                    && receipt.attempts.len() == 1
            );
            let rows = host.records("owned_block_control_v1").await?;
            ensure!(rows.len() == 1);
            let observations = rows[0]["report"]["observations"]["sessions"]
                .as_object()
                .context("cohort")?;
            ensure!(observations.len() == 1);
            let observed = observations.values().next().context("actual family")?;
            // Three uncached tokens plus two reads, three writes and two output
            // tokens: 12,43,83 are independently calculated, not nominal_cost().
            ensure!(
                observed["quality"] == 1.0 && observed["total_cost_micro_usd"].as_u64() == expected,
                "{name}: wrong original tariff evidence: {observed}"
            );
            ensure!(rows[0]["state"]["blocks"]["same-block"]["status"] != "adopted");
            ensure!(
                host.upstream
                    .received_requests()
                    .await
                    .context("actual provider")?
                    .len()
                    == 1
                    && host.fixture.service.current().is_ok()
            );
            Ok(())
        })
        .catch_unwind()
        .await;
        let checked = match checked {
            Ok(value) => value,
            Err(_) => Err(anyhow::anyhow!("pricing observation unwound")),
        };
        host.finish(checked)
            .await
            .with_context(|| format!("actual tariff case {name}"))?;
    }
    Ok(())
}
