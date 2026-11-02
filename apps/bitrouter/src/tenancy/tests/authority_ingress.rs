//! Final ingress gates over admitted leases, with no provider network I/O.

use super::{Fixture, failure, result};
use crate::{
    gateway::pipeline::{
        routing::{RoutingPrefs, RoutingTable},
        types::RoutingTarget,
    },
    tenancy::{
        Service, actor::AuthoritySubject, command::Command, execution::TenantExecution,
        resources::Resources, store::StoredResponse,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        authority::{CredentialIssued, OwnerKind, ResourceKind},
        configuration::ResourceRef,
        execution::{Outcome, ReceiptState},
        operation::{ErrorCode, Mutation},
    },
    types::ApiProtocol,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const MODEL: &str = "authority-ingress";
const TIMEOUT: Duration = Duration::from_secs(10);

mod reuse;

fn code(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("authority ingress failure: {error:?}")
}

/// UT: read saturation refuses work before constructing its owned payload.
#[tokio::test]
async fn saturated_read_does_not_prepare_payload_and_releases_failed_preparation() -> Result<()> {
    let fixture = Fixture::new().await?;
    let service = &fixture.service;
    let available = service.reads.available_permits();
    ensure!(available > 0);
    let held = service
        .reads
        .clone()
        .try_acquire_many_owned(u32::try_from(available)?)?;
    let prepared = AtomicBool::new(false);
    let denied = service
        .admitted_authority_read(|| {
            prepared.store(true, Ordering::Release);
            Err(ErrorCode::InvalidRequest.into())
        })
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::CapacityExceeded));
    ensure!(
        !prepared.load(Ordering::Acquire),
        "denied read prepared a payload"
    );
    drop(held);

    let failed = service
        .admitted_authority_read(|| {
            prepared.store(true, Ordering::Release);
            Err(ErrorCode::InvalidRequest.into())
        })
        .await;
    ensure!(matches!(failed, Err(error) if error.0.code == ErrorCode::InvalidRequest));
    ensure!(prepared.load(Ordering::Acquire));
    ensure!(service.reads.available_permits() == available);
    let authenticated = service
        .admitted_authority_read(|| Ok(AuthoritySubject::Caller(fixture.operator.clone())))
        .await
        .map_err(failure)?;
    drop(authenticated);
    fixture.service.shutdown().await?;
    Ok(())
}

struct Ready {
    fixture: Fixture,
    #[cfg(all(feature = "core", feature = "harness"))]
    route: ResourceRef,
    #[cfg(all(feature = "core", feature = "harness"))]
    slot: ResourceRef,
    credential: CredentialIssued,
    headers: http::HeaderMap,
}

impl Ready {
    async fn new(oauth: bool) -> Result<Self> {
        Self::with_queries(oauth, None).await
    }

    async fn with_queries(
        oauth: bool,
        queries: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Result<Self> {
        let mut fixture = Fixture::new().await?;
        fixture.service.shutdown().await?;
        if let Some(queries) = queries {
            fixture.db.set_metric_callback(move |info| {
                if info.statement.sql.trim_start().starts_with("SELECT") {
                    queries.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        let resources = Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?);
        let reference = |resource_kind, id: &str| -> Result<ResourceRef> {
            Ok(ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?,
                resource_kind,
                resource_id: id.try_into()?,
                revision_id: "r1".try_into()?,
            })
        };
        let route = reference(ResourceKind::Route, MODEL)?;
        let slot = reference(ResourceKind::Credential, "ingress-slot")?;
        let credential = if oauth {
            let registration =
                crate::gateway::providers::login::find("anthropic").context("registration")?;
            json!({"kind":"credential","credential":{"type":"oauth","data":{
                "access_token":"seed-access","refresh_token":"seed-refresh","expires_at":1}},
                "oauth":{"format":1,"mechanism":"claude_code_refresh_v1","provider":"claude-code",
                "account":"default","registration":"anthropic","token_endpoint":registration.auth.token_endpoint,
                "client_id":registration.auth.client_id}})
        } else {
            json!({"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-secret"}}})
        };
        resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
            "format":1,"installation_id":fixture.installation,"resources":[
                {"reference":slot,"definition":credential},
                {"reference":route,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,
                    "routing":{"providers":{"claude-code":{"api_base":"https://example.invalid/v1",
                        "api_protocol":[{"*":"messages"}],"models":[{"id":"model-a"}]}},
                        "routers":{"owned":{"selection":{"kind":"model","model":"claude-code:model-a"}}}},
                    "credentials":[{"provider":"claude-code","account":"default","credential":slot}]}}
            ]
        }))?).await?;
        fixture.service = Service::start_prepared(
            fixture.db.clone(),
            None,
            crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?,
            resources,
        )
        .await?;
        let mut input = fixture.configuration_input("ingress-config")?;
        input.document.routes.push(route.clone());
        let revision = match fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?
        {
            StoredResponse::Configuration(revision) => revision,
            _ => anyhow::bail!("expected retained configuration"),
        };
        let credential = fixture.issue("ingress-client").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let state = fixture.service.current().map_err(failure)?;
        let binding = fixture
            .service
            .capability_document(&state)
            .map_err(code)?
            .bind(revision.revision_id, "ingress-correlation".try_into()?)?;
        let mut headers = http::HeaderMap::new();
        for (name, value) in serde_json::to_value(binding)?
            .as_object()
            .context("dispatch binding")?
        {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes())?,
                value.as_str().context("binding text")?.parse()?,
            );
        }
        headers.insert(
            "authorization",
            format!(
                "Bearer {}",
                credential.secret.as_deref().context("issuance secret")?
            )
            .parse()?,
        );
        Ok(Self {
            fixture,
            #[cfg(all(feature = "core", feature = "harness"))]
            route,
            #[cfg(all(feature = "core", feature = "harness"))]
            slot,
            credential,
            headers,
        })
    }

    async fn execution(&self) -> Result<Arc<TenantExecution>> {
        let submission = self
            .fixture
            .service
            .submission(&self.headers)
            .await
            .map_err(failure)?;
        self.fixture
            .service
            .admit_execution(submission, MODEL, &ApiProtocol::Messages)
            .await
            .map_err(failure)
    }
}

async fn target(execution: &TenantExecution) -> Result<RoutingTarget> {
    execution
        .route()
        .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
        .await?
        .into_iter()
        .next()
        .context("retained candidate")
}

/// UT: a durable paid completion keeps its admitted grant but observes quarantine.
#[tokio::test]
async fn paid_delivery_keeps_admitted_grant_but_denies_authority_fence() -> Result<()> {
    use crate::tenancy::receipts::evidence::Protocol;

    let ready = Ready::new(false).await?;
    let service = &ready.fixture.service;
    let execution = ready.execution().await?;
    let target = target(&execution).await?;
    // Inject adapter observations into the real receipt journal; this tests the
    // delivery gate after durable settlement, not provider transport behavior.
    let mut attempt = execution.reserve_attempt(&target).map_err(code)?;
    let stamp = attempt.stamp().clone();
    execution
        .arm_attempt(&target, &mut attempt, &stamp)
        .map_err(code)?;
    attempt.http_status(200)?;
    attempt.observe_usage(Protocol::Messages, &json!({
        "input_tokens":3,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0
    }))?;
    attempt.validated_completion(true)?;
    drop(attempt);
    let owner = execution.receipt_owner()?.clone();
    tokio::time::timeout(TIMEOUT, owner.finish(Outcome::Completed))
        .await?
        .map_err(code)?;
    ensure!(owner.durable());
    let receipt = crate::tenancy::receipts::store::lookup(
        &ready.fixture.db,
        &ready.fixture.installation,
        &ready.fixture.tenant,
        execution.id(),
    )
    .await?
    .context("durable paid receipt")?;
    // Messages does not report a qualified reasoning-token dimension.
    ensure!(receipt.state == ReceiptState::Incomplete && receipt.outcome == Outcome::Completed);
    ensure!(!receipt.usage.complete && receipt.usage.reasoning_tokens.is_none());
    ensure!(receipt.attempts.len() == 1 && receipt.usage.input_tokens == Some(3));
    result(
        service
            .execute(
                ready.fixture.operator.clone(),
                Command::Revoke(
                    ready.fixture.tenant.clone(),
                    ready.credential.credential_id.clone(),
                    Mutation {
                        operation_id: "revoke-after-paid-completion".try_into()?,
                        expected_revision: ready.credential.credential_revision.clone(),
                        installation_id: ready.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(execution.revalidate() == Err(ErrorCode::Unauthorized));
    execution.check_delivery().map_err(code)?;
    // Exercise the exact liveness fence used by Proof::quarantine without
    // inventing ambiguous SQL custody after an already-acknowledged COMMIT.
    service.cleanup.fenced();
    ensure!(execution.check_delivery() == Err(ErrorCode::AuthorityUnavailable));
    owner.release_active().map_err(code)?;
    drop((owner, execution));
    tokio::time::timeout(TIMEOUT, service.shutdown()).await??;
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
mod assessment {
    use super::*;
    use crate::tenancy::{
        learning::source::Binding,
        oauth::{
            exchange::{Exchange, Operation},
            storage,
        },
    };

    fn arm(
        exchange: Arc<Exchange>,
        request: reqwest::Request,
    ) -> Result<(
        std::result::Result<(), ErrorCode>,
        std::result::Result<(), ErrorCode>,
    )> {
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let revalidated = exchange.execution.revalidate();
            // Always exercise the real arm, even when the owner check failed.
            let armed = exchange.execution.arm_oauth(&exchange, &request);
            send.send((revalidated, armed))
                .map_err(|_| anyhow::anyhow!("arm observer disconnected"))
        });
        // A recursive lock on the aliased owner/child lease must fail with a
        // bounded timeout rather than hang the test runtime's worker shutdown.
        let result = receive
            .recv_timeout(TIMEOUT)
            .context("OAuth arm deadlocked")?;
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("arm thread failed"))??;
        Ok(result)
    }

    async fn check(closed: bool) -> Result<()> {
        let ready = Ready::new(true).await?;
        let service = &ready.fixture.service;
        let authority = service
            .session_authority(&ready.headers, "ingress-owner")
            .await
            .map_err(failure)?;
        let descriptor = authority.descriptor().clone();
        let grant = bitrouter_ai::core::protocol::OwnershipGrant {
            session_id: descriptor.session.clone(),
            harness_id: descriptor.harness.clone(),
            core_instance_id: "ingress-core".into(),
            execution_epoch: 1,
        };
        let binding = authority
            .bind(descriptor, grant)
            .map_err(|error| anyhow::anyhow!("native binding: {error:?}"))?;
        // admit_assessment clones this host's Submission, so owner and child
        // share one live lease. This is the real aliased construction path.
        let execution = authority
            .host()
            .admit_assessment(Binding::Native(binding.clone()), &ready.route)
            .await
            .map_err(failure)?;
        let target = target(&execution).await?;
        let row = storage::load(&ready.fixture.db, &ready.fixture.installation, &ready.slot)
            .await?
            .context("OAuth seed")?;
        let request = reqwest::Client::new()
            .post(&row.metadata.profile.token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", "seed-refresh"),
                ("client_id", row.metadata.profile.client_id.as_str()),
            ])
            .build()?;
        let exchange = Exchange::new(execution.clone(), target, row).map_err(code)?;
        exchange.reserve().map_err(code)?;
        tokio::time::timeout(TIMEOUT, exchange.submit(Operation::Start))
            .await?
            .map_err(code)?;
        ensure!(!exchange.armed.load(Ordering::Acquire));
        if closed {
            binding
                .close_new_work()
                .map_err(|error| anyhow::anyhow!("close: {error:?}"))?;
        }
        let (revalidated, observed) = arm(exchange.clone(), request)?;
        ensure!(
            revalidated
                == if closed {
                    Err(ErrorCode::AuthorityUnavailable)
                } else {
                    Ok(())
                }
        );
        // Native assessment authority can be valid while its retained route
        // remains outside the supported Wire(Messages) OAuth consumer profile.
        ensure!(
            observed
                == if closed {
                    Err(ErrorCode::AuthorityUnavailable)
                } else {
                    Err(ErrorCode::CapabilityMismatch)
                }
        );
        ensure!(!exchange.armed.load(Ordering::Acquire));
        tokio::time::timeout(TIMEOUT, exchange.submit(Operation::Release))
            .await?
            .map_err(code)?;
        let owner = execution.receipt_owner()?.clone();
        tokio::time::timeout(TIMEOUT, owner.finish_or_join(Outcome::Cancelled))
            .await?
            .map_err(code)?;
        owner.release_active().map_err(code)?;
        drop((exchange, execution, owner, binding, authority));
        tokio::time::timeout(TIMEOUT, service.shutdown()).await??;
        Ok(())
    }

    /// UT: healthy aliased authority revalidates; native OAuth remains unsupported.
    #[tokio::test]
    async fn healthy_assessment_revalidates_aliased_lease_but_refuses_native_oauth() -> Result<()> {
        check(false).await
    }

    /// UT: closing after durable Start prevents OAuth refresh arm on that lease.
    #[tokio::test]
    async fn closed_assessment_after_oauth_start_cannot_arm() -> Result<()> {
        check(true).await
    }
}
