//! Accepted ledger grants are intersections, independent of host-control authority.
use super::*;
use bitrouter_ai::{
    control::ControlScope,
    eval::{AdmissionStatus, EvaluatorKind},
    tenancy::{
        authority::{AuditReaderCreate, OwnerKind, ResourceKey, ResourceKind},
        operation::{DispatchState, Error as TenancyError},
    },
};
use sea_orm::{ConnectionTrait, Statement};
use std::sync::Arc;

const BOUND: Duration = Duration::from_secs(25);
const FREEZE: &[u8] = br#"{"frozen_at":"2026-10-25T00:02:00Z"}"#;

async fn denied(response: axum::response::Response, code: ErrorCode) -> Result<()> {
    let expected = match code {
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::CapabilityMismatch => StatusCode::CONFLICT,
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        _ => StatusCode::FORBIDDEN,
    };
    ensure!(
        response.status() == expected,
        "unexpected denial status: {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get("cache-control")
            .is_some_and(|value| value == "no-store")
    );
    let bytes = axum::body::to_bytes(response.into_body(), 4096).await?;
    let error: TenancyError = serde_json::from_slice(&bytes)?;
    ensure!(error.code == code);
    ensure!(error.operation_id.is_none() && error.execution_id.is_none());
    ensure!(error.dispatch_state == DispatchState::NotStarted);
    Ok(())
}

async fn seed(fixture: &Fixture, router: &Router, secret: &str) -> Result<EvalSnapshot> {
    let _: PutSubjectReport = json(
        router
            .clone()
            .oneshot(request(
                fixture,
                secret,
                "POST",
                "subjects",
                serde_json::to_vec(&subject()?)?,
            )?)
            .await?,
    )
    .await?;
    let outcome: AdmissionOutcome = json(
        router
            .clone()
            .oneshot(request(
                fixture,
                secret,
                "POST",
                "results",
                serde_json::to_vec(&evaluation(fixture, EvalVerdict::Pass, "authority-pass")?)?,
            )?)
            .await?,
    )
    .await?;
    ensure!(outcome.status == AdmissionStatus::Admitted && !outcome.duplicate);
    json(
        router
            .clone()
            .oneshot(request(
                fixture,
                secret,
                "POST",
                "snapshots",
                FREEZE.to_vec(),
            )?)
            .await?,
    )
    .await
}

async fn matrix(
    fixture: &Fixture,
    router: &Router,
    secret: &str,
    snapshot: &EvalSnapshot,
    read: bool,
    write: bool,
) -> Result<()> {
    let cases = [
        ("GET", "subjects".to_owned(), Vec::new(), read),
        ("GET", "subjects/owned-subject".to_owned(), Vec::new(), read),
        ("GET", "status".to_owned(), Vec::new(), read),
        (
            "GET",
            format!("snapshots/{}", snapshot.evidence_root),
            Vec::new(),
            read,
        ),
        (
            "POST",
            "subjects".to_owned(),
            serde_json::to_vec(&subject()?)?,
            write,
        ),
        (
            "POST",
            "results".to_owned(),
            serde_json::to_vec(&evaluation(fixture, EvalVerdict::Pass, "authority-pass")?)?,
            write,
        ),
        (
            "POST",
            "snapshots".to_owned(),
            FREEZE.to_vec(),
            read && write,
        ),
    ];
    for (method, suffix, body, allowed) in cases {
        let response = router
            .clone()
            .oneshot(request(fixture, secret, method, &suffix, body)?)
            .await?;
        if allowed {
            let _: serde_json::Value = json(response).await?;
        } else {
            denied(response, ErrorCode::Forbidden).await?;
        }
    }
    ensure!(fixture.service.eval.leases.available_permits() == 8);
    ensure!(fixture.service.current().is_ok());
    Ok(())
}

async fn binding(
    fixture: &Fixture,
    role: TenantRole,
    scope: GrantScope,
    operation: &str,
) -> Result<()> {
    let current = fixture.service.current().map_err(failure)?;
    let previous = current
        .bindings
        .get(&(fixture.tenant.clone(), fixture.binding.clone()))
        .context("binding")?;
    let input = BindingPut {
        operation_id: operation.try_into()?,
        expected_revision: Some(previous.revision.clone()),
        principal_id: fixture.principal.clone(),
        role,
        scope,
        expires_at: previous.expires.wire.clone(),
        expected_management_owner: previous
            .managed_by
            .as_ref()
            .map(|owner| owner.principal.clone()),
        installation_id: fixture.installation.clone(),
    };
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), input),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(())
}

fn resource_scope(fixture: &Fixture, name: &str) -> Result<GrantScope> {
    Ok(GrantScope::Resources {
        resources: vec![ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: name.try_into()?,
        }],
    })
}

#[tokio::test]
async fn every_ledger_route_rechecks_each_current_binding_role() -> Result<()> {
    tokio::time::timeout(BOUND, async {
        let fixture = Fixture::new().await?;
        let credential = fixture.issue("role-ceiling-key").await?;
        let secret = credential.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        let snapshot = seed(&fixture, &router, secret).await?;
        // This original credential retains admin permissions. The binding is
        // changed through real CAS commands; cached credential grants cannot win.
        for (name, role, write) in [
            ("viewer", TenantRole::Viewer, false),
            ("editor", TenantRole::Editor, true),
            ("operator", TenantRole::TenantOperator, false),
            ("admin", TenantRole::TenantAdmin, true),
        ] {
            binding(
                &fixture,
                role,
                GrantScope::Tenant {
                    operator_pools: Vec::new(),
                },
                name,
            )
            .await?;
            matrix(&fixture, &router, secret, &snapshot, true, write).await?;
        }
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn credential_permissions_remain_a_ceiling_under_an_admin_binding() -> Result<()> {
    tokio::time::timeout(BOUND, async {
        for (name, permissions, read, write) in [
            ("only-read", vec![Permission::ResourceRead], true, false),
            (
                "only-submit",
                vec![Permission::EvaluationSubmit],
                false,
                true,
            ),
            (
                "only-execute",
                vec![Permission::ExecutionSubmit],
                false,
                false,
            ),
            (
                "read-submit",
                vec![Permission::ResourceRead, Permission::EvaluationSubmit],
                true,
                true,
            ),
        ] {
            let fixture = Fixture::new().await?;
            let admin = fixture.issue("seed-admin").await?;
            let router = router(&fixture)?;
            let snapshot = seed(
                &fixture,
                &router,
                admin.secret.as_deref().context("admin secret")?,
            )
            .await?;
            let mut input = fixture.issuance(name)?;
            input.permissions = permissions;
            let limited = issued(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        Command::Issue(fixture.tenant.clone(), input),
                    )
                    .await
                    .map_err(failure)?,
            )?;
            matrix(
                &fixture,
                &router,
                limited.secret.as_deref().context("limited secret")?,
                &snapshot,
                read,
                write,
            )
            .await?;
            fixture.service.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn resource_only_and_disjoint_grants_cannot_be_combined_into_ledger_authority() -> Result<()>
{
    tokio::time::timeout(BOUND, async {
        let fixture = Fixture::new().await?;
        let tenant = fixture.issue("tenant-grant").await?;
        let router = router(&fixture)?;
        let snapshot = seed(
            &fixture,
            &router,
            tenant.secret.as_deref().context("tenant secret")?,
        )
        .await?;
        let mut limited = fixture.issuance("resource-grant")?;
        limited.scope = resource_scope(&fixture, "route-a")?;
        let limited = issued(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), limited),
                )
                .await
                .map_err(failure)?,
        )?;
        let secret = limited.secret.as_deref().context("resource secret")?;
        // Broad binding plus narrow credential does not authorize the ledger.
        matrix(&fixture, &router, secret, &snapshot, false, false).await?;
        binding(
            &fixture,
            TenantRole::TenantAdmin,
            resource_scope(&fixture, "route-a")?,
            "matching-resource-binding",
        )
        .await?;
        matrix(
            &fixture,
            &router,
            tenant.secret.as_deref().context("tenant secret")?,
            &snapshot,
            false,
            false,
        )
        .await?;
        matrix(&fixture, &router, secret, &snapshot, false, false).await?;
        binding(
            &fixture,
            TenantRole::TenantAdmin,
            resource_scope(&fixture, "route-b")?,
            "disjoint-resource-binding",
        )
        .await?;
        // A second whole-tenant binding for the same principal is not the binding
        // selected by either credential; it must not be unioned into their grants.
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Binding(
                        fixture.tenant.clone(),
                        "unselected-full-binding".try_into()?,
                        BindingPut {
                            operation_id: "create-unselected-full-binding".try_into()?,
                            expected_revision: None,
                            principal_id: fixture.principal.clone(),
                            role: TenantRole::TenantAdmin,
                            scope: GrantScope::Tenant {
                                operator_pools: Vec::new(),
                            },
                            expires_at: expiry()?,
                            expected_management_owner: None,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        matrix(&fixture, &router, secret, &snapshot, false, false).await?;
        matrix(
            &fixture,
            &router,
            tenant.secret.as_deref().context("tenant secret")?,
            &snapshot,
            false,
            false,
        )
        .await?;
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn host_operator_and_receipt_audit_tokens_do_not_become_ordinary_ledger_credentials()
-> Result<()> {
    tokio::time::timeout(BOUND, async {
        let fixture = Fixture::new().await?;
        let ordinary = fixture.issue("ordinary-ledger-key").await?;
        let operator_token = "eval-operator-token-with-at-least-32-bytes";
        let host_token = "eval-host-reader-token-with-at-least-32-bytes";
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![
                    crate::config::ControlCredentialConfig {
                        id: "test-operator".into(),
                        token_env: "EVAL_OPERATOR".into(),
                        scopes: vec![ControlScope::Tenancy],
                    },
                    crate::config::ControlCredentialConfig {
                        id: "host-reader".into(),
                        token_env: "EVAL_HOST_READER".into(),
                        scopes: vec![ControlScope::Read],
                    },
                ],
                ..Default::default()
            },
            |name| {
                Some(
                    if name == "EVAL_OPERATOR" {
                        operator_token
                    } else {
                        host_token
                    }
                    .into(),
                )
            },
        )?;
        let router = crate::tenancy::http::router(fixture.service.clone(), auth);
        // Prove this configured installation token works on its intended branch.
        let management = Request::builder()
            .uri(format!(
                "/control/v1/tenants/{}/credentials/{}",
                fixture.tenant, ordinary.credential_id
            ))
            .header("authorization", format!("Bearer {operator_token}"))
            .header("bitrouter-installation-id", fixture.installation.as_str())
            .body(Body::empty())?;
        let response = router.clone().oneshot(management).await?;
        ensure!(response.status() == StatusCode::OK);
        drop(axum::body::to_bytes(response.into_body(), 4096).await?);
        let snapshot = seed(
            &fixture,
            &router,
            ordinary.secret.as_deref().context("ordinary secret")?,
        )
        .await?;
        let snapshot_path = format!("snapshots/{}", snapshot.evidence_root);
        let audit = issued(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::AuditReader(
                        fixture.tenant.clone(),
                        AuditReaderCreate {
                            operation_id: "ledger-audit-reader".try_into()?,
                            principal_id: fixture.principal.clone(),
                            expected_authority_revision: fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .revision,
                            expires_at: expiry()?,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        for (token, code) in [
            (operator_token, ErrorCode::Unauthorized),
            (host_token, ErrorCode::Unauthorized),
            (
                audit.secret.as_deref().context("audit secret")?,
                ErrorCode::Forbidden,
            ),
        ] {
            for (method, suffix, body) in [
                ("GET", "status", Vec::new()),
                ("GET", "subjects", Vec::new()),
                ("GET", "subjects/owned-subject", Vec::new()),
                ("GET", snapshot_path.as_str(), Vec::new()),
                ("POST", "subjects", serde_json::to_vec(&subject()?)?),
                (
                    "POST",
                    "results",
                    serde_json::to_vec(&evaluation(
                        &fixture,
                        EvalVerdict::Pass,
                        "forbidden-authority",
                    )?)?,
                ),
                ("POST", "snapshots", FREEZE.to_vec()),
            ] {
                denied(
                    router
                        .clone()
                        .oneshot(request(&fixture, token, method, suffix, body)?)
                        .await?,
                    code,
                )
                .await?;
            }
        }
        ensure!(admission_count(&fixture).await? == 1);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn result_provenance(fixture: &Fixture) -> Result<Vec<(String, String)>> {
    let rows = fixture.db.query_all(Statement::from_sql_and_values(
        fixture.db.get_database_backend(),
        "SELECT content_digest, creator_principal_id FROM eval_results WHERE installation_id = ? AND tenant_id = ? ORDER BY result_id",
        [fixture.installation.as_str().into(), fixture.tenant.as_str().into()],
    )).await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "content_digest")?,
                row.try_get("", "creator_principal_id")?,
            ))
        })
        .collect()
}

async fn admission_count(fixture: &Fixture) -> Result<i64> {
    let row = fixture.db.query_one(Statement::from_sql_and_values(
        fixture.db.get_database_backend(),
        "SELECT COUNT(*) AS total FROM eval_admission_events WHERE installation_id = ? AND tenant_id = ?",
        [fixture.installation.as_str().into(), fixture.tenant.as_str().into()],
    )).await?.context("admission count")?;
    Ok(row.try_get("", "total")?)
}

async fn second_tenant(first: &Fixture) -> Result<Fixture> {
    let tenant: TenantId = "tenant-b".try_into()?;
    let principal: PrincipalId = "principal-b".try_into()?;
    let binding: BindingId = "binding-b".try_into()?;
    result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Tenant(
                    tenant.clone(),
                    TenantPut {
                        operation_id: "eval-create-b".try_into()?,
                        expected_revision: None,
                        state: TenantState::Active,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Principal(
                    principal.clone(),
                    PrincipalPut {
                        operation_id: "eval-create-b-principal".try_into()?,
                        expected_revision: None,
                        issuer: "test".into(),
                        subject: "eval-other-tenant".into(),
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let binding_result = result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "eval-bind-b".try_into()?,
                        expected_revision: None,
                        principal_id: principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: GrantScope::Tenant {
                            operator_pools: Vec::new(),
                        },
                        expires_at: expiry()?,
                        expected_management_owner: None,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(Fixture {
        service: first.service.clone(),
        db: first.db.clone(),
        operator: first.operator.clone(),
        installation: first.installation.clone(),
        tenant,
        principal,
        binding,
        binding_result,
    })
}

#[tokio::test]
async fn rotated_principal_replays_without_new_events_but_foreign_tenant_body_is_not_adopted()
-> Result<()> {
    tokio::time::timeout(BOUND, async {
        let fixture = Fixture::new().await?;
        let original = fixture.issue("rotation-original").await?;
        let router = router(&fixture)?;
        let snapshot = seed(
            &fixture,
            &router,
            original.secret.as_deref().context("original secret")?,
        )
        .await?;
        let before = result_provenance(&fixture).await?;
        ensure!(before.len() == 1 && before[0].1 == fixture.principal.as_str());
        let admissions = admission_count(&fixture).await?;
        let mut replacement = fixture.issuance("rotation-replacement")?;
        replacement.rotate_credential_id = Some(original.credential_id.clone());
        let replacement = issued(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), replacement),
                )
                .await
                .map_err(failure)?,
        )?;
        let secret = replacement
            .secret
            .as_deref()
            .context("replacement secret")?;
        let body = evaluation(&fixture, EvalVerdict::Pass, "authority-pass")?;
        denied(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    original.secret.as_deref().context("old secret")?,
                    "POST",
                    "results",
                    serde_json::to_vec(&body)?,
                )?)
                .await?,
            ErrorCode::Unauthorized,
        )
        .await?;
        let replay: AdmissionOutcome = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&body)?,
                )?)
                .await?,
        )
        .await?;
        ensure!(replay.duplicate && replay.status == AdmissionStatus::Admitted);
        ensure!(result_provenance(&fixture).await? == before);
        ensure!(admission_count(&fixture).await? == admissions);
        let other = second_tenant(&fixture).await?;
        let other_key = other.issue("foreign-tenant-key").await?;
        let other_secret = other_key.secret.as_deref().context("other secret")?;
        // Valid B authority cannot read A's path or adopt A's evaluator identity,
        // even after a same-name B subject has been imported successfully.
        denied(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    other_secret,
                    "GET",
                    "subjects/owned-subject",
                    Vec::new(),
                )?)
                .await?,
            ErrorCode::Forbidden,
        )
        .await?;
        let _: PutSubjectReport = json(
            router
                .clone()
                .oneshot(request(
                    &other,
                    other_secret,
                    "POST",
                    "subjects",
                    serde_json::to_vec(&subject()?)?,
                )?)
                .await?,
        )
        .await?;
        denied(
            router
                .clone()
                .oneshot(request(
                    &other,
                    other_secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&body)?,
                )?)
                .await?,
            ErrorCode::Forbidden,
        )
        .await?;
        let foreign_fail = EvaluationResult {
            verdict: EvalVerdict::Fail,
            ..body.clone()
        };
        denied(
            router
                .clone()
                .oneshot(request(
                    &other,
                    other_secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&foreign_fail)?,
                )?)
                .await?,
            ErrorCode::Forbidden,
        )
        .await?;
        denied(
            router
                .clone()
                .oneshot(request(
                    &other,
                    other_secret,
                    "GET",
                    &format!("snapshots/{}", snapshot.evidence_root),
                    Vec::new(),
                )?)
                .await?,
            ErrorCode::NotFound,
        )
        .await?;
        ensure!(result_provenance(&other).await?.is_empty() && admission_count(&other).await? == 0);
        // The same logical idempotency key is independently usable by B only
        // with B's authenticated principal and B's own immutable closure.
        let own: AdmissionOutcome = json(
            router
                .clone()
                .oneshot(request(
                    &other,
                    other_secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&evaluation(&other, EvalVerdict::Fail, "authority-pass")?)?,
                )?)
                .await?,
        )
        .await?;
        ensure!(
            !own.duplicate
                && own.status == AdmissionStatus::Admitted
                && own.result_id != replay.result_id
        );
        ensure!(result_provenance(&fixture).await? == before);
        ensure!(admission_count(&fixture).await? == admissions);
        let unchanged: EvalSnapshot = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "GET",
                    &format!("snapshots/{}", snapshot.evidence_root),
                    Vec::new(),
                )?)
                .await?,
        )
        .await?;
        ensure!(unchanged == snapshot);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn non_generic_kinds_and_hard_violations_cannot_acquire_host_evaluator_rank() -> Result<()> {
    tokio::time::timeout(BOUND, async {
        let fixture = Fixture::new().await?;
        let key = fixture.issue("fixed-profile-key").await?;
        let secret = key.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        let snapshot = seed(&fixture, &router, secret).await?;
        for kind in [
            EvaluatorKind::TaskNative,
            EvaluatorKind::Human,
            EvaluatorKind::Enterprise,
            EvaluatorKind::Agentic,
        ] {
            let mut input = evaluation(&fixture, EvalVerdict::Fail, "profile-forgery")?;
            input.evaluator.kind = kind;
            denied(
                router
                    .clone()
                    .oneshot(request(
                        &fixture,
                        secret,
                        "POST",
                        "results",
                        serde_json::to_vec(&input)?,
                    )?)
                    .await?,
                ErrorCode::Forbidden,
            )
            .await?;
        }
        let mut input = evaluation(&fixture, EvalVerdict::Fail, "hard-violation-forgery")?;
        input.hard_violations.push("fixture-hard-failure".into());
        denied(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&input)?,
                )?)
                .await?,
            ErrorCode::Forbidden,
        )
        .await?;
        ensure!(admission_count(&fixture).await? == 1);
        ensure!(result_provenance(&fixture).await?.len() == 1);
        let current: EvalSnapshot = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "snapshots",
                    FREEZE.to_vec(),
                )?)
                .await?,
        )
        .await?;
        ensure!(current == snapshot);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn sealed_eval_identity_cannot_cross_a_service_restart_with_the_same_database() -> Result<()>
{
    use crate::tenancy::eval::{Action, Job, Request as EvalRequest, WORK};
    use crate::tenancy::transaction::{Disposition, Phase};
    tokio::time::timeout(BOUND, async {
        let mut fixture = Fixture::new().await?;
        let key = fixture.issue("incarnation-key").await?;
        let secret = key.secret.as_deref().context("secret")?;
        let ingress = request(&fixture, secret, "GET", "status", Vec::new())?;
        let identity = fixture
            .service
            .eval_identity(ingress.headers(), fixture.tenant.clone(), Action::Read)
            .map_err(failure)?;
        let installation = fixture.installation.clone();
        fixture.service.shutdown().await?;
        fixture.service = Service::open(fixture.db.clone(), None).await?;
        ensure!(fixture.service.current().map_err(failure)?.installation == installation);
        ensure!(fixture.service.authenticate(secret).is_ok());
        let gate = fixture.service.cleanup.hold(Phase::Beginning);
        let (reply, receive) = tokio::sync::oneshot::channel();
        let rejected = fixture.service.enqueue_eval(Job {
            identity: identity.clone(),
            request: EvalRequest::Status,
            deadline: tokio::time::Instant::now() + WORK,
            lease: fixture.service.eval_lease().map_err(failure)?,
            reply,
        });
        ensure!(matches!(rejected, Err(error) if error.0.code == ErrorCode::CapabilityMismatch));
        ensure!(receive.await.is_err());
        // Also reach the actor's independent origin checkpoint, keeping the real
        // sealed proof but bypassing only the already-tested enqueue checkpoint.
        let (reply, receive) = tokio::sync::oneshot::channel();
        fixture
            .service
            .commands
            .try_send(crate::tenancy::actor::Job::Eval(Box::new(Job {
                identity,
                request: EvalRequest::Status,
                deadline: tokio::time::Instant::now() + WORK,
                lease: fixture.service.eval_lease().map_err(failure)?,
                reply,
            })))
            .map_err(|_| anyhow::anyhow!("fixture ordinary queue unavailable"))?;
        let rejected = tokio::time::timeout(Duration::from_secs(3), receive).await??;
        ensure!(matches!(rejected, Err(error) if error.0.code == ErrorCode::CapabilityMismatch));
        ensure!(fixture.service.cleanup.disposition() == Disposition::NoTransaction);
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        drop(gate);
        fixture.service.cleanup.release_gate();
        let status: StatusReport = json(
            router(&fixture)?
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?,
        )
        .await?;
        ensure!(status.subjects == 0 && status.results == 0);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn held_transaction_rechecks_expiry_and_scope_without_revision_change() -> Result<()> {
    use crate::tenancy::{
        model::Deadline,
        transaction::{Disposition, Phase},
    };
    tokio::time::timeout(BOUND, async {
        for change in ["credential-expiry", "binding-expiry", "binding-scope"] {
            let fixture = Fixture::new().await?;
            let key = fixture.issue(change).await?;
            let secret = key.secret.as_deref().context("secret")?;
            let before = fixture.service.current().map_err(failure)?.revision;
            let gate = fixture.service.cleanup.hold(Phase::Active);
            let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
                router(&fixture)?.oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "subjects",
                    serde_json::to_vec(&subject()?)?,
                )?),
            ));
            tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
            ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(fixture.service.eval.leases.available_permits() == 7);
            // Existing expiry tests use this private snapshot technique. It moves
            // the checked grant across a barrier without sleeps or a public clock
            // API; it does not claim management publishes during a serialized tx.
            let expired = Deadline::parse(
                (Utc::now() - chrono::Duration::seconds(1))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    .try_into()?,
            )?;
            {
                let mut published = fixture
                    .service
                    .snapshot
                    .write()
                    .map_err(|_| anyhow::anyhow!("authority snapshot lock"))?;
                let state = Arc::make_mut(&mut *published);
                match change {
                    "credential-expiry" => {
                        state
                            .credentials
                            .get_mut(&key.credential_id)
                            .context("credential")?
                            .expires = expired
                    }
                    "binding-expiry" => {
                        state
                            .bindings
                            .get_mut(&(fixture.tenant.clone(), fixture.binding.clone()))
                            .context("binding")?
                            .expires = expired
                    }
                    _ => {
                        state
                            .bindings
                            .get_mut(&(fixture.tenant.clone(), fixture.binding.clone()))
                            .context("binding")?
                            .scope = resource_scope(&fixture, "no-ledger")?
                    }
                }
            }
            fixture.service.cleanup.release_gate();
            let code = if change == "credential-expiry" {
                ErrorCode::Unauthorized
            } else {
                ErrorCode::Forbidden
            };
            denied(task.await??, code).await?;
            ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
            ensure!(fixture.service.current().map_err(failure)?.revision == before);
            ensure!(fixture.service.eval.leases.available_permits() == 8);
            let row = fixture
                .db
                .query_one(Statement::from_string(
                    fixture.db.get_database_backend(),
                    "SELECT COUNT(*) AS total FROM eval_subjects".to_owned(),
                ))
                .await?
                .context("subject count")?;
            ensure!(row.try_get::<i64>("", "total")? == 0);
            fixture.service.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
