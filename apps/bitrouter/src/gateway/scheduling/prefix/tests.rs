use super::*;
use crate::{
    config::{
        Config,
        inference_routing::{DestinationConfig, InferenceRoutingConfig, Policy},
        routing_table::resolve_route_chain,
    },
    gateway::{
        caller::CallerContext,
        pipeline::{routing::RoutingPrefs, types::PipelineRequest},
        scheduling::{
            Scheduler,
            observation::Shape,
            queue::{Identity, Request},
        },
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{Content, Prompt};
use tokio_util::sync::CancellationToken;
fn input() -> Result<Prompt> {
    Ok(serde_json::from_value(
        serde_json::json!({"model":"model", "system":"system prefix ".repeat(300),
        "messages":[{"role":"user","content":[{"type":"text","text":"user prefix ".repeat(100)}]}],"stream":false}),
    )?)
}
fn target() -> Result<RoutingTarget> {
    let config: Config = serde_json::from_value(
        serde_json::json!({"inherit_defaults":false,"providers":{"fixture":{
        "api_base":"https://fixture.invalid","api_key":"secret-one","models":[{"id":"model"}],"api_protocol":[{"*":"chat_completions"}]}}}),
    )?;
    resolve_route_chain(&config, "fixture:model", &RoutingPrefs::default())?
        .pop()
        .context("target")
}
fn settings() -> InferenceRoutingConfig {
    InferenceRoutingConfig {
        policy: Policy::PrefixAffinity,
        destinations: vec![DestinationConfig {
            id: "fixture".into(),
            provider: "fixture".into(),
            model: "model".into(),
            endpoint: "https://fixture.invalid".into(),
            account: None,
            max_active: 4,
        }],
        ..Default::default()
    }
}
fn binding(
    scheduler: &Scheduler,
    target: &RoutingTarget,
    prompt: Prompt,
    user: &str,
) -> Result<Binding> {
    let caller = if user == "local" {
        CallerContext::local()
    } else {
        CallerContext::new("key", user)
    };
    let ctx = PipelineContext::new(PipelineRequest::new("model", caller, prompt));
    scheduler
        .projector
        .as_ref()
        .context("projector")?
        .prepare(std::slice::from_ref(target), &ctx)
        .pop()
        .flatten()
        .context("supported projection")
}
async fn book(
    scheduler: &Scheduler,
    target: &RoutingTarget,
    binding: Binding,
) -> Result<super::super::Reservation> {
    let (shape, work) = Shape::from_prompt(&input()?)?;
    Ok(scheduler
        .enqueue(
            Identity::Local,
            target,
            Request {
                candidate: scheduler.candidate(target, 0)?,
                work,
                shape,
                deadline: Instant::now() + Duration::from_secs(60),
                wait_until: Instant::now() + Duration::from_secs(10),
                retained_bytes: 100,
                planning: None,
                prefix: Some(binding),
            },
        )?
        .receive(CancellationToken::new())
        .await?)
}
#[tokio::test(start_paused = true)]
async fn prefix_reservation_siblings_self_credit_and_evidence_transitions() -> Result<()> {
    let scheduler = Scheduler::new(&settings(), 1)?;
    let target = target()?;
    let b = binding(&scheduler, &target, input()?, "a")?;
    ensure!(scheduler.inspect_prefix(&b, None).entries == 0);
    let first = book(&scheduler, &target, b.clone()).await?;
    let second = book(&scheduler, &target, b.clone()).await?;
    let view = scheduler.inspect_prefix(&b, None);
    ensure!(view.tentative == b.input.blocks.len() * 2 && view.affinity_millionths == 0);
    drop(first);
    ensure!(scheduler.inspect_prefix(&b, None).tentative == b.input.blocks.len());
    let observer = second.observer();
    observer.begin_send(&target)?;
    let recent = scheduler.inspect_prefix(&b, None);
    ensure!(recent.affinity_millionths > 0 && recent.affinity_millionths <= 250000);
    ensure!(
        scheduler
            .inspect_prefix(&b, Some(observer.id))
            .affinity_millionths
            == 0
    );
    observer.headers(200);
    observer.completed(true);
    drop(second);
    let success = scheduler.inspect_prefix(&b, None);
    ensure!(
        success.affinity_millionths > recent.affinity_millionths
            && success.affinity_millionths <= 750000
    );
    let abandoned = book(&scheduler, &target, b.clone()).await?;
    abandoned.observer().begin_send(&target)?;
    drop(abandoned);
    let mixed = scheduler.inspect_prefix(&b, None);
    ensure!(mixed.unknown > 0 && mixed.affinity_millionths == success.affinity_millionths);
    let rejected = book(&scheduler, &target, b.clone()).await?;
    rejected.observer().begin_send(&target)?;
    rejected.observer().headers(429);
    drop(rejected);
    ensure!(scheduler.inspect_prefix(&b, None).affinity_millionths == success.affinity_millionths);
    tokio::time::advance(Duration::from_secs(60)).await;
    ensure!(
        scheduler.inspect_prefix(&b, None).affinity_millionths == success.affinity_millionths / 2
    );
    scheduler.shutdown().await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn prefix_isolates_tenants_credentials_models_adapters_generations_and_restart() -> Result<()>
{
    let scheduler = Scheduler::new(&settings(), 1)?;
    let mut target = target()?;
    let original = binding(&scheduler, &target, input()?, "a")?;
    let reservation = book(&scheduler, &target, original.clone()).await?;
    reservation.observer().begin_send(&target)?;
    reservation.observer().headers(200);
    reservation.observer().completed(true);
    drop(reservation);
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, input()?, "b")?, None)
            .matched_bytes
            == 0
    );
    target.api_key = "secret-two".into();
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, input()?, "a")?, None)
            .matched_bytes
            == 0
    );
    target.api_key = "secret-one".into();
    target.service_id = "other-version".into();
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, input()?, "a")?, None)
            .matched_bytes
            == 0
    );
    target.service_id = "model".into();
    target.api_base = "https://another-deployment.invalid".into();
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, input()?, "a")?, None)
            .matched_bytes
            == 0
    );
    target.api_base = "https://fixture.invalid".into();
    target.account_label = Some("another-account".into());
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, input()?, "a")?, None)
            .matched_bytes
            == 0
    );
    target.account_label = None;
    target.service_id = "model".into();
    target.api_protocol = bitrouter_ai::types::ApiProtocol::Messages;
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, input()?, "a")?, None)
            .matched_bytes
            == 0
    );
    let restarted = Scheduler::new(&settings(), 2)?;
    target.api_protocol = bitrouter_ai::types::ApiProtocol::ChatCompletions;
    let fresh = binding(&restarted, &target, input()?, "a")?;
    ensure!(
        fresh.input.blocks != original.input.blocks
            && restarted.inspect_prefix(&fresh, None).matched_bytes == 0
    );
    restarted.shutdown().await?;
    scheduler.shutdown().await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn prefix_complete_blocks_truncation_boundaries_controls_and_no_query_warming() -> Result<()>
{
    let mut config = settings();
    config.prefix.max_projection_bytes = 1024;
    let scheduler = Scheduler::new(&config, 1)?;
    let target = target()?;
    let b = binding(&scheduler, &target, input()?, "a")?;
    ensure!(b.input.blocks.len() == 4 && b.input.bytes > 1024);
    let reservation = book(&scheduler, &target, b.clone()).await?;
    reservation.observer().begin_send(&target)?;
    reservation.observer().headers(200);
    reservation.observer().completed(true);
    drop(reservation);
    let view = scheduler.inspect_prefix(&b, None);
    ensure!(view.matched_bytes == 1024 && view.affinity_millionths < 750000);
    let before = scheduler
        .inner
        .lock()
        .prefix
        .as_ref()
        .context("index")?
        .tick;
    for _ in 0..10 {
        ensure!(scheduler.inspect_prefix(&b, None) == view);
    }
    ensure!(
        scheduler
            .inner
            .lock()
            .prefix
            .as_ref()
            .context("index")?
            .tick
            == before
    );
    let mut changed = input()?;
    changed.params.temperature = Some(0.5);
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, changed, "a")?, None)
            .matched_bytes
            == 0
    );
    let mut changed = input()?;
    changed.system = Some("different ".repeat(300));
    ensure!(
        scheduler
            .inspect_prefix(&binding(&scheduler, &target, changed, "a")?, None)
            .matched_bytes
            == 0
    );
    let mut transport = input()?;
    transport.stream = true;
    transport.model = "alias".into();
    transport.params.store = Some(false);
    ensure!(binding(&scheduler, &target, transport, "a")?.input.blocks == b.input.blocks);
    scheduler.shutdown().await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn prefix_ttl_and_memory_pressure_never_release_live_ownership() -> Result<()> {
    let mut config = settings();
    config.prefix.ttl_ms = 50;
    config.prefix.max_entries = 2;
    config.prefix.max_projection_bytes = 1024;
    config.prefix.max_bytes = (INDEX_BASE_BYTES + ENTRY_BYTES * 2) as u32;
    let scheduler = Scheduler::new(&config, 1)?;
    let target = target()?;
    let b = binding(&scheduler, &target, input()?, "a")?;
    let reservation = book(&scheduler, &target, b.clone()).await?;
    let (shape, _) = Shape::from_prompt(&input()?)?;
    let view = scheduler.inspect_prefix(&b, None);
    ensure!(
        view.entries > 0 && view.entries <= 2 && view.bytes <= u64::from(config.prefix.max_bytes)
    );
    tokio::time::advance(Duration::from_millis(51)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    ensure!(
        scheduler.inspect_prefix(&b, None).entries == 0 && scheduler.inspect(shape)[0].active == 1
    );
    reservation.observer().begin_send(&target)?;
    reservation.observer().headers(200);
    reservation.observer().completed(true);
    drop(reservation);
    ensure!(scheduler.inspect_prefix(&b, None).entries <= 2);
    scheduler.shutdown().await?;
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn prefix_unsupported_inputs_and_cached_measurements_do_not_invent_residency() -> Result<()> {
    let scheduler = Scheduler::new(&settings(), 1)?;
    let target = target()?;
    let anonymous = PipelineContext::new(PipelineRequest::new(
        "model",
        CallerContext::anonymous(),
        input()?,
    ));
    ensure!(
        scheduler
            .projector
            .as_ref()
            .context("projector")?
            .prepare(std::slice::from_ref(&target), &anonymous)
            .is_empty()
    );
    let mut opaque = input()?;
    opaque
        .params
        .extra
        .insert("unknown_provider_field".into(), true.into());
    let ctx = PipelineContext::new(PipelineRequest::new(
        "model",
        CallerContext::local(),
        opaque,
    ));
    ensure!(
        scheduler
            .projector
            .as_ref()
            .context("projector")?
            .prepare(std::slice::from_ref(&target), &ctx)
            .is_empty()
    );
    let mut image = input()?;
    image.messages[0].content.push(Content::File {
        media_type: "image/png".into(),
        data: bitrouter_ai::types::DataContent::Url {
            url: "https://image.invalid".into(),
        },
        filename: None,
        provider_metadata: Default::default(),
    });
    ensure!(binding(&scheduler, &target, image, "a").is_err());
    let b = binding(&scheduler, &target, input()?, "a")?;
    let reservation = book(&scheduler, &target, b.clone()).await?;
    let observer = reservation.observer();
    observer.begin_send(&target)?;
    let mut usage = Usage {
        prompt_tokens: 8,
        cache_read_tokens: 4,
        origin: UsageOrigin::Estimated,
        ..Default::default()
    };
    observer.usage(&usage);
    ensure!(
        scheduler
            .inspect_prefix(&b, None)
            .last_cached_tokens
            .is_none()
    );
    usage.origin = UsageOrigin::ProviderReported;
    observer.usage(&usage);
    ensure!(scheduler.inspect_prefix(&b, None).last_cached_tokens == Some((Some(4), None)));
    let recent = scheduler.inspect_prefix(&b, None).affinity_millionths;
    usage.cache_read_tokens = 0;
    usage.raw = None;
    observer.usage(&usage);
    ensure!(scheduler.inspect_prefix(&b, None).last_cached_tokens == Some((Some(4), None)));
    usage.raw = Some(Box::new(
        serde_json::json!({"prompt_tokens_details":{"cached_tokens":0}}),
    ));
    observer.usage(&usage);
    ensure!(
        scheduler.inspect_prefix(&b, None).last_cached_tokens == Some((Some(0), None))
            && scheduler.inspect_prefix(&b, None).affinity_millionths == recent
    );
    drop(reservation);
    ensure!(scheduler.inspect_prefix(&b, None).affinity_millionths == 0);
    scheduler.shutdown().await?;
    Ok(())
}

#[test]
fn prefix_projection_versions_content_boundaries_and_tool_preludes_are_distinct() -> Result<()> {
    let key = [7u8; 32];
    let tenant = [9u8; 32];
    let make = |prompt: &Prompt, version: &[u8]| {
        projection::build(prompt, &key, &tenant, 32768, version, 0).context("projection")
    };
    let mut a = input()?;
    a.system = None;
    a.messages[0].content = vec![Content::Text {
        text: "abc ".repeat(300),
        provider_metadata: Default::default(),
    }];
    let original = make(&a, b"adapter.v1")?;
    ensure!(original.blocks != make(&a, b"adapter.v2")?.blocks);
    let mut b = a.clone();
    b.messages[0].content = vec![
        Content::Text {
            text: "a".into(),
            provider_metadata: Default::default(),
        },
        Content::Text {
            text: format!("bc {}", "abc ".repeat(299)),
            provider_metadata: Default::default(),
        },
    ];
    let changed = make(&b, b"adapter.v1")?;
    let first = original
        .blocks
        .iter()
        .zip(&changed.blocks)
        .position(|(a, b)| a != b)
        .context("boundary changed no digest")?;
    ensure!(
        original.blocks[first..]
            .iter()
            .zip(&changed.blocks[first..])
            .all(|(a, b)| a != b)
    );
    a.tools.push(bitrouter_ai::types::Tool::Function {
        name: "first-tool".into(),
        description: None,
        parameters: serde_json::json!({"type":"object"}),
        strict: None,
        provider_metadata: Default::default(),
    });
    let one = make(&a, b"adapter.v1")?;
    let bitrouter_ai::types::Tool::Function { name, .. } = &mut a.tools[0] else {
        anyhow::bail!("tool");
    };
    *name = "other-tool".into();
    ensure!(one.blocks[0] != make(&a, b"adapter.v1")?.blocks[0]);
    let mut huge = input()?;
    huge.system = Some("tail ".repeat(1_000_000));
    let limited = projection::build(&huge, &key, &tenant, 256, b"adapter.v1", 0)
        .context("limited projection")?;
    ensure!(limited.blocks.len() == 1 && limited.bytes > 5_000_000);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reloaded_ttl_cannot_extend_prior_generation_evidence() -> Result<()> {
    let mut config = settings();
    config.prefix.ttl_ms = 50;
    let scheduler = Scheduler::new(&config, 1)?;
    let target = target()?;
    let old = binding(&scheduler, &target, input()?, "a")?;
    let reservation = book(&scheduler, &target, old.clone()).await?;
    reservation.observer().begin_send(&target)?;
    reservation.observer().headers(200);
    reservation.observer().completed(true);
    drop(reservation);
    ensure!(scheduler.inspect_prefix(&old, None).matched_bytes > 0);
    config.prefix.ttl_ms = 5000;
    let next = scheduler.snapshot(&config, 2);
    ensure!(old.ttl_ms == 50);
    let fresh = binding(&next, &target, input()?, "a")?;
    ensure!(fresh.ttl_ms == 5000 && fresh.namespace != old.namespace);
    tokio::time::advance(Duration::from_millis(51)).await;
    ensure!(next.inspect_prefix(&old, None).matched_bytes == 0);
    scheduler.shutdown().await?;
    Ok(())
}
