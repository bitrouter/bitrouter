//! Assembly boundary tests. The shared fixture invokes the installed callbacks;
//! it does not substitute for the separate ingress/receiver process campaign.

use std::any::TypeId;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{GenerationParams, Message, ModelOperation, Prompt, Role, UsageOrigin};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use super::{Assembled, Config};
use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::execution::Pipeline;
use crate::gateway::pipeline::types::PipelineRequest;

fn combine(results: impl IntoIterator<Item = Result<()>>) -> Result<()> {
    let errors = results
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

fn context(id: &str, model: &str) -> PipelineContext {
    let prompt = Prompt {
        model: model.into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "assembly fixture")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let mut request = PipelineRequest::new(
        model,
        CallerContext::new("fixture-key", "fixture-owner"),
        prompt,
    );
    request.request_id = id.into();
    PipelineContext::new(request)
}

fn hook_types(pipeline: &Pipeline) -> Vec<TypeId> {
    let mut ids = Vec::new();
    ids.extend(
        pipeline
            .pre_resolution_hooks
            .iter()
            .map(|hook| hook.type_id),
    );
    ids.extend(
        pipeline
            .router_preparation_hooks
            .iter()
            .map(|hook| hook.type_id),
    );
    ids.extend(pipeline.pre_request_hooks.iter().map(|hook| hook.type_id));
    ids.extend(pipeline.route_hooks.iter().map(|hook| hook.type_id));
    ids.extend(pipeline.observe_hooks.iter().map(|hook| hook.type_id));
    ids.extend(
        pipeline
            .settlement_recorders
            .iter()
            .map(|hook| hook.type_id),
    );
    ids
}

fn assert_writer_mode(pipeline: &Pipeline, shared: bool) {
    let ids = hook_types(pipeline);
    for local in [
        TypeId::of::<super::MeteringRecorder>(),
        TypeId::of::<super::PolicySettlementRecorder>(),
        TypeId::of::<crate::gateway::observability::binding::CanonicalRequestBinding>(),
        TypeId::of::<crate::gateway::routing::blocks::runtime::BlockPolicyRouter>(),
        TypeId::of::<crate::gateway::observability::requests::GatewayRequestTracker>(),
        TypeId::of::<crate::gateway::metering::judge::JudgeCosts>(),
    ] {
        assert_eq!(ids.contains(&local), !shared);
    }
    assert_eq!(pipeline.native_cost_source.is_some(), !shared);
    assert_eq!(pipeline.native_cost_estimator.is_some(), !shared);
    for operation in [ModelOperation::Generation, ModelOperation::Decisions] {
        assert!(
            pipeline
                .route_hooks
                .iter()
                .any(|hook| hook.type_id == TypeId::of::<super::CaptureTariffs>()
                    && hook.supports(operation))
        );
        assert!(
            pipeline
                .pre_request_hooks
                .iter()
                .any(|hook| hook.type_id == TypeId::of::<super::PolicyHook>()
                    && hook.supports(operation))
        );
    }
}

async fn record(assembled: &Assembled, context: &mut PipelineContext) -> Result<()> {
    let mut settlement = context.settlement_context();
    settlement.provider_id = "vendor".into();
    settlement.model_id = "model".into();
    settlement.prompt_tokens = 10;
    settlement.completion_tokens = 5;
    settlement.usage_origin = UsageOrigin::ProviderReported;
    settlement.request_duration_ms = 100;
    for recorder in &assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .settlement_recorders
    {
        if recorder.supports(settlement.operation) {
            recorder.record(&mut settlement).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn local_assembly_keeps_durable_hooks_and_metering() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config = crate::config::parse_with(
        "inherit_defaults: false\nregistry:\n  enabled: false\ndatabase:\n  url: 'sqlite::memory:'\n",
        |_| None,
    )?;
    let assembled =
        super::build_app_with_path(&config, Some(&home.path().join("bitrouter.yaml"))).await?;
    let result = async {
        let pipeline = assembled.app.language_model().context("pipeline missing")?;
        assert_writer_mode(pipeline, false);
        let mut request = context("local-writer-positive", "vendor:model");
        record(&assembled, &mut request).await?;
        let row = assembled
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM requests WHERE request_id = 'local-writer-positive'"
                    .to_owned(),
            ))
            .await?
            .context("metering count missing")?;
        ensure!(row.try_get::<i64>("", "count")? == 1);
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let drained = assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .drain_required_pending_settlements()
        .await;
    combine([
        result.context("local writer exercise"),
        drained.map(|_| ()).context("local settlement cleanup"),
    ])
}

#[cfg(feature = "multi-instance")]
mod shared {
    use super::super::shared_writers::{DiscardPending, RequestScope, validate_policies};
    use super::*;
    use crate::gateway::observability::decision::EvalInvocation;
    use crate::gateway::pipeline::hooks::{HookDecision, PreRequestHook};
    use crate::gateway::pipeline::owned_tasks::{Drain as PipelineDrain, Summary};
    use crate::gateway::pipeline::routing::ModelSelector;
    use crate::gateway::shared::continuation::SharedContinuationDb;
    use crate::gateway::shared::telemetry::Telemetry;
    use crate::policy_lock::{PolicyDefinition, PolicyLock};
    use futures::FutureExt as _;
    use sea_orm::{ConnectOptions, Database, DatabaseConnection};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::{collections::BTreeMap, panic::AssertUnwindSafe, sync::Arc, time::Duration};
    use tokio::time::Instant;

    #[tokio::test]
    async fn unsupported_claims_and_counter_policies_fail_without_database_access() -> Result<()> {
        let policy = crate::policy::Policy {
            id: "fixture".into(),
            ..Default::default()
        };
        validate_policies(std::slice::from_ref(&policy))?;
        for policy in [
            crate::policy::Policy {
                max_spend_micro_usd: Some(0),
                ..policy.clone()
            },
            crate::policy::Policy {
                max_requests_per_minute: Some(1),
                ..policy
            },
        ] {
            let error = validate_policies(&[policy])
                .err()
                .context("counter policy accepted")?;
            ensure!(
                error.to_string()
                    == "shared access policy cannot use local spend/rate counters; configure shared admission limits"
            );
        }
        for model in ["vendor:model", "@coding", "bitrouter/coding"] {
            let mut ctx = context("ordinary", model);
            ensure!(matches!(
                RequestScope.check(&mut ctx).await?,
                HookDecision::Allow
            ));
            ensure!(matches!(
                RequestScope.revalidate_context(&ctx).await?,
                HookDecision::Allow
            ));
        }
        let mut judge = context("brjudge_reserved", "vendor:model");
        ensure!(RequestScope.check(&mut judge).await.is_err());
        ensure!(RequestScope.revalidate_context(&judge).await.is_err());
        let mut request = PipelineRequest::new(
            "vendor:model",
            CallerContext::local(),
            context("temporary", "vendor:model")
                .require_generation_prompt()?
                .clone(),
        );
        request.headers.insert(
            "x-bitrouter-controller-id",
            http::HeaderValue::from_static(""),
        );
        let mut controller = PipelineContext::new(request);
        ensure!(RequestScope.check(&mut controller).await.is_err());
        ensure!(RequestScope.revalidate_context(&controller).await.is_err());
        Ok(())
    }

    fn config(namespace: &str, trajectory: bool) -> Result<Config> {
        crate::config::parse_with(
            &format!(
                r#"
inherit_defaults: false
registry:
  enabled: false
database:
  url: postgres://127.0.0.1/not-opened
gateway_state:
  mode: shared
  namespace: {namespace}
  deployment_key_env: UNUSED_WRITERS_KEY
trajectory:
  enabled: {trajectory}
providers:
  vendor:
    api_base: https://fixture.invalid/v1
    api_protocol:
      - '*': chat_completions
    models:
      - id: model
        pricing:
          input_micro_usd_per_token: 1.0
          output_micro_usd_per_token: 2.0
presets:
  coding:
    model: vendor:model
    policy: coding
"#
            ),
            |_| None,
        )
        .map_err(Into::into)
    }

    fn lock() -> PolicyLock {
        let mut document = PolicyLock::default();
        document.policies.insert(
            "coding".into(),
            PolicyDefinition {
                tiers: BTreeMap::from([("economy".into(), "vendor:model".into())]),
                default_tier: Some("economy".into()),
                tool_use_tier: Some("economy".into()),
                tool_safe_tiers: vec!["economy".into()],
                ..Default::default()
            },
        );
        document
    }

    async fn counts(db: &DatabaseConnection) -> Result<Vec<i64>> {
        let mut counts = Vec::new();
        for table in [
            "requests",
            "trajectory_episodes",
            "trajectory_events",
            "trajectory_requests",
            "trajectory_prefix_index",
            "trajectory_outbox",
            "eval_subjects",
            "eval_results",
            "eval_admission_events",
            "eval_snapshots",
            "checkpoint_evolution",
            "acp_capture_connections",
            "acp_canonical_sessions",
            "acp_capture_events",
        ] {
            let row = db
                .query_one(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("SELECT COUNT(*) AS count FROM {table}"),
                ))
                .await?
                .context("legacy table count missing")?;
            counts.push(row.try_get("", "count")?);
        }
        Ok(counts)
    }

    async fn exercise(assembled: &Assembled, config: &Config) -> Result<()> {
        let pipeline = assembled.app.language_model().context("pipeline missing")?;
        assert_writer_mode(pipeline, true);
        ensure!(pipeline.settlement_recorders.len() == 1);
        ensure!(pipeline.settlement_recorders[0].type_id == TypeId::of::<DiscardPending>());
        ensure!(pipeline.pre_resolution_hooks[0].type_id == TypeId::of::<RequestScope>());
        #[cfg(feature = "core")]
        ensure!(assembled.trajectory_outbox_publisher.is_none());
        // Named policy selection still runs against its published document. No
        // SharedAdmission is forged here; receiver/guard integration has its own fixture.
        let mut ctx = context("shared-writer-fixture", "vendor:model");
        assembled
            .policy_runtime
            .prepare_selection("coding", None, &mut ctx)
            .await?;
        assembled
            .policy_runtime
            .select_variant("coding", None, &mut ctx)
            .await?;
        ensure!(ctx.model() == "vendor:model");
        let invocation = ctx
            .get_event::<EvalInvocation>()
            .context("policy invocation missing")?
            .clone();
        ensure!(
            assembled
                .pending_eval_decisions
                .peek(&invocation, "fixture-owner")
                .is_some()
        );
        let chain = crate::config::routing_table::resolve_route_chain(
            config,
            "vendor:model",
            &Default::default(),
        )?;
        ensure!(chain.len() == 1);
        let capture = pipeline
            .route_hooks
            .iter()
            .find(|hook| hook.type_id == TypeId::of::<super::super::CaptureTariffs>())
            .context("tariff hook missing")?;
        capture.after_resolve(&chain, &mut ctx).await?;
        let tariff = ctx
            .get_event::<crate::metering::tariff::TargetTariffSnapshot>()
            .context("dispatch tariff missing")?;
        let pricing = tariff
            .tariff
            .pricing
            .as_ref()
            .context("frozen prices missing")?;
        ensure!(
            pricing.input_micro_usd_per_token == Some(1.0)
                && pricing.output_micro_usd_per_token == Some(2.0)
        );
        record(assembled, &mut ctx).await?;
        ensure!(assembled.pending_eval_decisions.is_empty());
        Ok(())
    }

    #[derive(Default)]
    struct Cleanup {
        pipeline: Option<Summary>,
        native: Option<crate::continuation::owned::Drain>,
        errors: Vec<String>,
    }

    impl Cleanup {
        fn check(&self) -> Result<()> {
            let mut errors = self.errors.clone();
            match self.pipeline {
                Some(summary) if summary.is_clean() => {}
                summary => errors.push(format!("pipeline cleanup was not clean: {summary:?}")),
            }
            match self.native {
                Some(report) if report.is_clean() => {}
                report => errors.push(format!("native cleanup was not clean: {report:?}")),
            }
            ensure!(errors.is_empty(), "{}", errors.join("\n"));
            Ok(())
        }
    }

    // The caller retains Assembled through every join. There are no transport
    // roots here; the only probe source is dropped with its borrowed exercise.
    // Forced joins deliberately have no second timeout that could drop owners.
    async fn cleanup(assembled: &Assembled, deadline: Instant) -> Cleanup {
        let mut report = Cleanup::default();
        let registry = &assembled.continuation_registry;
        let mut native_force = None;
        if let Some(tasks) = assembled
            .app
            .language_model()
            .and_then(|pipeline| pipeline.owned_tasks())
        {
            tasks.close_sources();
            match tasks.drain_until(deadline).await {
                Ok(PipelineDrain::Joined(summary)) => report.pipeline = Some(summary),
                Ok(PipelineDrain::Retained(_)) => {}
                Err(error) => report.errors.push(format!("pipeline grace: {error:#}")),
            }
            if report.pipeline.is_none() {
                // Broadcast to both groups before awaiting either forced join.
                native_force = Some(registry.request_force_owned());
                match tasks.force() {
                    Ok(()) => match tasks.join_forced().await {
                        Ok(summary) => report.pipeline = Some(summary),
                        Err(error) => {
                            report
                                .errors
                                .push(format!("pipeline forced join: {error:#}"));
                        }
                    },
                    Err(error) => report.errors.push(format!("pipeline force: {error:#}")),
                }
            }
        } else {
            report
                .errors
                .push("shared pipeline task owner missing".into());
            native_force = Some(registry.request_force_owned());
        }

        // Only a completed pipeline grace reaches native graceful cleanup.
        // A failed/retained pipeline uses native force, which starts no DB work.
        if native_force.is_none() {
            match registry.drain_owned_until(deadline).await {
                Ok(native) if native.is_clean() => report.native = Some(native),
                Ok(native) => {
                    report.errors.push(format!("native grace: {native:?}"));
                    native_force = Some(registry.request_force_owned());
                }
                Err(error) => {
                    report.errors.push(format!("native grace: {error:#}"));
                    native_force = Some(registry.request_force_owned());
                }
            }
        }
        if let Some(requested) = native_force {
            match requested {
                Ok(()) => match registry.join_forced_owned().await {
                    Ok(native) => report.native = Some(native),
                    Err(error) => report.errors.push(format!("native forced join: {error:#}")),
                },
                Err(error) => report.errors.push(format!("native force: {error:#}")),
            }
        }
        report
    }

    async fn build_fixture(
        capability: &SharedContinuationDb,
        telemetry: &Arc<Telemetry>,
        trajectory: bool,
    ) -> Result<(Assembled, Config)> {
        let inputs = super::super::SharedAssembly {
            continuation: capability.fixture_peer().await?,
            telemetry: telemetry.clone(),
            auth_appliers: crate::gateway::providers::auth::AuthAppliers::new(),
            correlation_key: crate::trajectory::canonical::CorrelationKey::from_bytes(
                rand::random(),
            )?,
            policies: Vec::new(),
            policy_lock: Some(lock()),
        };
        let config = config(capability.namespace(), trajectory)?;
        // Construction is not inside a cancellable exercise timeout. Shared
        // assembly does not start its native worker until it is used.
        let assembled = super::super::build_shared_app(&config, &inputs).await?;
        Ok((assembled, config))
    }

    async fn timed_exercise(assembled: &Assembled, config: &Config) -> Result<()> {
        // Assertions inside exercise must also leave the owner available for
        // ordered cleanup. Destroy any panic payload before joining descendants.
        match AssertUnwindSafe(tokio::time::timeout(
            Duration::from_secs(45),
            exercise(assembled, config),
        ))
        .catch_unwind()
        .await
        {
            Ok(result) => result.context("writer assembly exercise timed out")?,
            Err(payload) => {
                drop(payload);
                anyhow::bail!("writer assembly exercise panicked");
            }
        }
    }

    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    async fn timeout_probe(assembled: &Assembled, dropped: &Arc<AtomicBool>) -> Result<()> {
        let tasks = assembled
            .app
            .language_model()
            .context("probe pipeline missing")?
            .owned_tasks()
            .context("probe task owner missing")?;
        assembled.continuation_registry.start_reconciler();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task_dropped = Dropped(dropped.clone());
        tasks.spawn_execution(async move {
            let _dropped = task_dropped;
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        tokio::time::timeout(Duration::from_secs(3), ready).await??;
        let expired = {
            let exercise = async {
                let _source = tasks.acquire()?;
                std::future::pending::<Result<()>>().await
            };
            tokio::pin!(exercise);
            // Establish source ownership before expiring the borrowed wait.
            ensure!(futures::poll!(&mut exercise).is_pending());
            tokio::time::timeout_at(Instant::now(), &mut exercise).await
        };
        ensure!(expired.is_err(), "probe exercise did not time out");
        let pending = tasks.pending();
        ensure!(pending.sources == 0 && pending.executions == 1);
        ensure!(
            !dropped.load(Ordering::SeqCst),
            "probe task was lost on timeout"
        );
        Ok(())
    }

    async fn unchanged(db: &DatabaseConnection, baseline: &[i64]) -> Result<()> {
        ensure!(
            counts(db).await?.as_slice() == baseline,
            "shared assembly wrote a legacy durable table"
        );
        Ok(())
    }

    async fn campaign(db: &DatabaseConnection, telemetry: &Arc<Telemetry>) -> Result<()> {
        crate::db::run_migrations(db).await?;
        let capability = SharedContinuationDb::fixture(db.clone()).await?;
        let baseline = counts(db).await?;
        ensure!(baseline.iter().all(|count| *count == 0));
        for trajectory in [false, true] {
            let (assembled, config) = build_fixture(&capability, telemetry, trajectory).await?;
            let result = timed_exercise(&assembled, &config).await;
            let cleaned = cleanup(&assembled, Instant::now() + Duration::from_secs(5)).await;
            combine([
                result.context("shared writer exercise"),
                cleaned.check().context("shared writer cleanup"),
                unchanged(db, &baseline).await,
            ])?;
        }

        // This is an assembly-owner probe, not an ingress/process acceptance
        // test. Use the same cleanup path with a real retained pending task.
        let (assembled, _) = build_fixture(&capability, telemetry, false).await?;
        let dropped = Arc::new(AtomicBool::new(false));
        let result = timeout_probe(&assembled, &dropped).await;
        let cleaned = cleanup(&assembled, Instant::now()).await;
        let proof = (|| {
            ensure!(cleaned.errors.is_empty(), "{}", cleaned.errors.join("\n"));
            let pipeline = cleaned.pipeline.context("probe pipeline join missing")?;
            ensure!(pipeline.forced && pipeline.joined == 1 && pipeline.cancelled == 1);
            ensure!(
                dropped.load(Ordering::SeqCst),
                "probe task was not destroyed"
            );
            let native = cleaned.native.context("probe native join missing")?;
            ensure!(native.worker.started && native.worker.forced && native.worker.joined);
            ensure!(native.markers.total == 0);
            ensure!(
                assembled
                    .continuation_registry
                    .owned_status()?
                    .is_quiescent()
            );
            let pending = assembled
                .app
                .language_model()
                .context("probe pipeline missing")?
                .owned_tasks()
                .context("probe task owner missing")?
                .pending();
            ensure!(pending.sources == 0 && pending.executions == 0 && pending.settlements == 0);
            // Joined forced work remains a dirty outcome, never a clean drain.
            ensure!(cleaned.check().is_err());
            Ok(())
        })();
        combine([
            result.context("owned-timeout probe"),
            proof.context("owned-timeout cleanup proof"),
            unchanged(db, &baseline).await,
        ])
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
    async fn postgres_shared_assembly_does_not_write_legacy_tables() -> Result<()> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
            .context("dedicated PostgreSQL URL required")?;
        let mut options = ConnectOptions::new(url.clone());
        options.max_connections(1).sqlx_logging(false);
        let setup = Database::connect(options).await?;
        let schema = format!("shared_writers_{}", uuid::Uuid::new_v4().simple());
        // Only this generated UUID identifier enters SQL; each fixture owns its schema.
        // https://www.postgresql.org/docs/current/sql-createschema.html
        let created = setup
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("CREATE SCHEMA {schema}"),
            ))
            .await;
        let result = if let Err(error) = created {
            Err(error).context("create owned writer schema")
        } else {
            let result = async {
                let mut options = ConnectOptions::new(url);
                options
                    .max_connections(4)
                    .sqlx_logging(false)
                    .set_schema_search_path(schema.clone());
                let telemetry = Arc::new(Telemetry::new(None)?);
                let (result, db) = match Database::connect(options).await {
                    Ok(db) => (campaign(&db, &telemetry).await, Some(db)),
                    Err(error) => (Err(error).context("connect writer schema"), None),
                };
                // Campaign cleanup has joined all assembly-owned tasks first.
                let stopped = telemetry.force_join().await.and_then(|report| {
                    ensure!(report.is_clean(), "writer telemetry cleanup was not clean");
                    Ok(())
                });
                let closed = match db {
                    Some(db) => db.close().await.context("close writer schema pool"),
                    None => Ok(()),
                };
                combine([result, stopped.context("writer telemetry cleanup"), closed])
            }
            .await;
            let removed = setup
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("DROP SCHEMA {schema} CASCADE"),
                ))
                .await
                .map(|_| ());
            combine([result, removed.context("remove owned writer schema")])
        };
        let closed = setup.close().await;
        combine([result, closed.context("close schema admin pool")])
    }
}
