//! Configured platform routes under real tenant authority and provider dispatch.

#![cfg(unix)]

mod auth;
mod defaults;
mod google_chat;
mod recovery;

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKey, ResourceKind},
        catalog::{CatalogCandidateV1, CatalogCandidatesV1, CatalogSelectionRequestV1},
        configuration::ResourceRef,
        execution::ExecutionReceipt,
        execution::Outcome,
        identity::ExecutionId,
        ordered::{GenerationProtocol, OrderedCandidateSelection},
        resources::{Definition, ResourcePut, ResourceRevoke},
        validation::Validate,
    },
    types::ApiProtocol,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::{Value, json};
use tokio_util::task::AbortOnDropHandle;
use tower::ServiceExt;

use super::*;
use crate::tenancy::{
    execution::{TenantExecution, cached::Caches},
    ownership::{Claim, Lease},
    resources::Resources,
};

fn accepted_selection(
    dispatch: &bitrouter_ai::tenancy::execution::Dispatch,
) -> Result<&bitrouter_ai::tenancy::ordered::AcceptedSelection> {
    match dispatch {
        bitrouter_ai::tenancy::execution::Dispatch::Ordered {
            accepted_selection, ..
        } => Ok(accepted_selection),
        bitrouter_ai::tenancy::execution::Dispatch::Strict => {
            anyhow::bail!("expected actual ordered dispatch")
        }
    }
}

const ROUTE: &str = "model.acme.shared";
const SOURCE: &str = "mock/default/acme/shared";
const SECRET: &str = "configured-upstream-secret";
const OWNED_SECRET: &str = "owned-catalog-upstream-secret";
const TIMEOUT: Duration = Duration::from_secs(10);

fn catalog_fixture_yaml(url: &str, platform_secret: &str) -> String {
    format!(
        r#"
inherit_defaults: false
server:
  skip_auth: false
control:
  enabled: true
  credentials:
    - id: catalog-operator
      token_env: CONFIGURED_CATALOG_OPERATOR_TOKEN
      scopes: [control:tenancy]
providers:
  mock:
    api_base: {url}
    api_key: {platform_secret:?}
    api_protocol: [{{"*": chat_completions}}]
    models: [{{id: acme/shared, provider_model_id: native-shared}}]
models:
  acme/shared:
    endpoints: [{{provider: mock, service_id: native-shared}}]
tenancy:
  enabled: true
  wrapping_key_env: CONFIGURED_CATALOG_WRAPPING_KEY
  catalog:
    pool_id: platform
    routes: {{{ROUTE}: acme/shared}}
    profiles:
      {SOURCE}:
        schema_version: 1
        rate: {{rate_group_id: upstream, member_rpm: 1000}}
        health: null
        health_group_id: null
        fallback: ordered_terminal_rejections_v1
"#
    )
}

#[test]
fn catalog_yaml_decodes_rate_and_fallback_profile_fields() -> Result<()> {
    let config = crate::config::parse(&catalog_fixture_yaml(
        "https://configured.example/v1",
        SECRET,
    ))?;
    let model = config
        .providers
        .get("mock")
        .context("decoded provider")?
        .model_config("native-shared")
        .context("canonical provider model")?;
    ensure!(
        model.id == "acme/shared" && model.provider_model_id.as_deref() == Some("native-shared")
    );
    let catalog = config.tenancy.catalog.context("decoded catalog")?;
    ensure!(catalog.pool_id == "platform");
    ensure!(
        catalog
            .routes
            .get(ROUTE)
            .is_some_and(|selector| selector == "acme/shared")
    );
    let profile = catalog
        .profiles
        .get(SOURCE)
        .context("decoded configured profile")?;
    profile.validate()?;
    ensure!(profile.schema_version == 1);
    let rate = profile
        .rate
        .as_ref()
        .context("configured rate was discarded")?;
    ensure!(rate.rate_group_id.as_str() == "upstream" && rate.member_rpm == 1000);
    ensure!(
        profile.observe_rate_group_id.is_none()
            && profile.health.is_none()
            && profile.health_group_id.is_none()
    );
    ensure!(
        profile.fallback
            == bitrouter_ai::tenancy::admission::FallbackRule::OrderedTerminalRejectionsV1
    );
    Ok(())
}

#[derive(Clone)]
struct ProviderState {
    calls: Arc<AtomicUsize>,
    sent: tokio::sync::mpsc::Sender<(String, Value)>,
    reject_owned: bool,
}

struct Provider {
    url: String,
    calls: Arc<AtomicUsize>,
    received: tokio::sync::mpsc::Receiver<(String, Value)>,
    _task: AbortOnDropHandle<std::io::Result<()>>,
}

impl Provider {
    async fn start() -> Result<Self> {
        Self::with_owned_rejection(false).await
    }

    async fn with_owned_rejection(reject_owned: bool) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
        let calls = Arc::new(AtomicUsize::new(0));
        let (sent, received) = tokio::sync::mpsc::channel(16);
        let router = Router::new().route("/v1/chat/completions", post(
            |State(state): State<ProviderState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                state.calls.fetch_add(1, Ordering::SeqCst);
                let authorization = headers.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or_default();
                let owned = authorization == format!("Bearer {OWNED_SECRET}");
                let encoded = body.to_string();
                if (!owned && authorization != format!("Bearer {SECRET}"))
                    || body["model"] != "native-shared"
                    || body.get(bitrouter_ai::tenancy::catalog::BODY_MEMBER).is_some()
                    || encoded.contains(SECRET) || encoded.contains(OWNED_SECRET)
                    || state.sent.try_send((authorization.to_owned(), body)).is_err()
                {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error":"wrong configured target"})));
                }
                if owned && state.reject_owned {
                    return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
                        "error":{"message":"qualified terminal rejection"},
                        "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
                    })));
                }
                (StatusCode::OK, Json(json!({
                    "id":"chatcmpl-configured", "object":"chat.completion", "model":"native-shared",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"shared reply"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
                })))
            }
        )).with_state(ProviderState { calls: calls.clone(), sent, reject_owned });
        let task =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            url,
            calls,
            received,
            _task: task,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn observed(&mut self, secret: &str) -> Result<()> {
        let (authorization, body) = tokio::time::timeout(TIMEOUT, self.received.recv())
            .await?
            .context("provider request missing")?;
        ensure!(
            authorization == format!("Bearer {secret}"),
            "wrong candidate credential or order"
        );
        ensure!(body["model"] == "native-shared");
        ensure!(
            body.get(bitrouter_ai::tenancy::catalog::BODY_MEMBER)
                .is_none()
        );
        let encoded = body.to_string();
        ensure!(!encoded.contains(SECRET) && !encoded.contains(OWNED_SECRET));
        Ok(())
    }
}

struct Harness {
    fixture: Fixture,
    config: crate::config::Config,
    key: [u8; 32],
    directory: tempfile::TempDir,
}

impl Harness {
    async fn new(url: &str) -> Result<Self> {
        Self::with_platform_secret(url, SECRET).await
    }

    async fn with_platform_secret(url: &str, platform_secret: &str) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let (db, owner, claim) = crate::db::connect_owned_tenancy(&format!(
            "sqlite://{}",
            directory.path().join("catalog.sqlite").display()
        ))
        .await?
        .claim()?;
        let mut fixture = Fixture::with_database(db, &mut |_| {}).await?;
        let config = crate::config::parse(&catalog_fixture_yaml(url, platform_secret))?;
        let key = rand::random();
        let before = publication_counts(&fixture).await?;
        start(&mut fixture, &config, key, owner, claim).await?;
        ensure!(
            publication_counts(&fixture).await? == before,
            "loading the platform catalog published tenant state or a management operation"
        );
        Ok(Self {
            fixture,
            config,
            key,
            directory,
        })
    }

    async fn restart(self) -> Result<Self> {
        let Self {
            fixture,
            config,
            key,
            directory,
        } = self;
        let before = publication_counts(&fixture).await?;
        let Fixture {
            service,
            db,
            operator,
            installation,
            tenant,
            principal,
            binding,
            binding_result,
            wrapping_key,
        } = fixture;
        service.shutdown().await?;
        drop(service);
        db.close().await?;
        let (db, owner, claim) = crate::db::connect_owned_tenancy(&format!(
            "sqlite://{}",
            directory.path().join("catalog.sqlite").display()
        ))
        .await?
        .claim()?;
        let fixture = Fixture {
            service: configured_service(&db, &installation, &config, key, owner, claim).await?,
            db,
            operator,
            installation,
            tenant,
            principal,
            binding,
            binding_result,
            wrapping_key,
        };
        ensure!(
            publication_counts(&fixture).await? == before,
            "restart republished the configured catalog into tenant state"
        );
        Ok(Self {
            fixture,
            config,
            key,
            directory,
        })
    }
}

async fn start(
    fixture: &mut Fixture,
    config: &crate::config::Config,
    key: [u8; 32],
    owner: Lease,
    claim: Claim,
) -> Result<()> {
    fixture.service.shutdown().await?;
    fixture.service = configured_service(
        &fixture.db,
        &fixture.installation,
        config,
        key,
        owner,
        claim,
    )
    .await?;
    Ok(())
}

async fn configured_service(
    db: &sea_orm::DatabaseConnection,
    installation: &InstallationId,
    config: &crate::config::Config,
    key: [u8; 32],
    owner: Lease,
    claim: Claim,
) -> Result<Service> {
    let mut service = Service::start_resources_with_continuations(
        db.clone(),
        Some(owner),
        super::super::store::load(db, Utc::now()).await?,
        Arc::new(Resources::with_key(installation.clone(), key)?),
        crate::tenancy::Provisioning {
            import: None,
            claim: Some(claim),
            continuations: &config.continuation,
            receipt_observer: crate::telemetry::receipt_latency::Sink::default(),
            config: &config.tenancy,
        },
        None,
    )
    .await?;
    if let Err(error) = service.configure_catalog(config).await {
        service.shutdown().await?;
        return Err(error);
    }
    service.caches = Arc::new(Caches::with_limits(
        1,
        128 * 1024 * 1024,
        1,
        8 * 1024 * 1024,
    )?);
    service.dispatch_test_ready.store(true, Ordering::Release);
    Ok(service)
}

async fn publication_counts(fixture: &Fixture) -> Result<Vec<i64>> {
    let mut counts = Vec::new();
    for table in [
        "tenancy_configurations",
        "tenancy_active_configurations",
        "tenancy_resource_revisions",
        "tenancy_resource_lifecycle",
        "tenancy_operations",
        "tenancy_tenants",
        "tenancy_principals",
    ] {
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS n FROM {table}"),
            ))
            .await?
            .context("publication count")?;
        counts.push(row.try_get("", "n")?);
    }
    Ok(counts)
}

async fn catalog(
    fixture: &Fixture,
    caller: &Caller,
    tenant: &TenantId,
) -> Result<CatalogCandidatesV1> {
    fixture
        .service
        .catalog_candidates(caller, tenant, ROUTE, GenerationProtocol::ChatCompletions)
        .await
        .map_err(failure)
}

fn scope(catalog: &CatalogCandidatesV1) -> GrantScope {
    let mut resources = vec![catalog.manifest.route.key()];
    for candidate in &catalog.manifest.candidates {
        for reference in
            std::iter::once(&candidate.credential).chain(candidate.admission_profile.iter())
        {
            if !resources.contains(&reference.key()) {
                resources.push(reference.key());
            }
        }
    }
    GrantScope::Resources { resources }
}

async fn grant(
    fixture: &mut Fixture,
    scope: GrantScope,
    permissions: Vec<Permission>,
    operation: &str,
) -> Result<CredentialIssued> {
    let Caller::Installation(owner) = &fixture.operator else {
        anyhow::bail!("installation operator missing");
    };
    fixture.binding_result = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    BindingPut {
                        operation_id: format!("{operation}-binding").try_into()?,
                        expected_revision: fixture.binding_result.resource_revision.clone(),
                        principal_id: fixture.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: scope.clone(),
                        expires_at: expiry()?,
                        expected_management_owner: Some(owner.clone()),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let mut input = fixture.issuance(operation).await?;
    input.scope = scope;
    input.permissions = permissions;
    issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )
}

async fn second_tenant(first: &Fixture) -> Result<Fixture> {
    let tenant: TenantId = "tenant-b".try_into()?;
    let principal: PrincipalId = "principal-b".try_into()?;
    let binding: BindingId = "binding-b".try_into()?;
    for command in [
        Command::Tenant(
            tenant.clone(),
            TenantPut {
                operation_id: "create-second-tenant".try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: first.installation.clone(),
            },
        ),
        Command::Principal(
            principal.clone(),
            PrincipalPut {
                operation_id: "create-second-principal".try_into()?,
                expected_revision: None,
                issuer: "test".into(),
                subject: "second-subject".into(),
                installation_id: first.installation.clone(),
            },
        ),
    ] {
        result(
            first
                .service
                .execute(first.operator.clone(), command)
                .await
                .map_err(failure)?,
        )?;
    }
    let binding_result = result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "create-second-binding".try_into()?,
                        expected_revision: None,
                        principal_id: principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: super::super::mutation::tenant_scope(),
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
        wrapping_key: first.wrapping_key,
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

async fn caller(service: &Service, credential: &CredentialIssued) -> Result<Caller> {
    service
        .authenticate(
            credential
                .secret
                .as_deref()
                .context("issued credential secret")?,
        )
        .await
        .map_err(failure)
}

async fn headers(
    fixture: &Fixture,
    credential: &CredentialIssued,
    catalog: &CatalogCandidatesV1,
) -> Result<HeaderMap> {
    manifest_headers(fixture, credential, &catalog.manifest).await
}

async fn manifest_headers(
    fixture: &Fixture,
    credential: &CredentialIssued,
    manifest: &bitrouter_ai::tenancy::ordered::CandidateManifest,
) -> Result<HeaderMap> {
    let capabilities = super::super::ordered::capabilities(
        State(fixture.service.clone()),
        Extension(fixture.operator.clone()),
    )
    .await
    .map_err(failure)?
    .0;
    let binding = capabilities.bind(
        manifest.execution_revision.clone(),
        "catalog-correlation".try_into()?,
        OrderedCandidateSelection {
            schema_version: 1,
            admission_mode: manifest.admission_mode,
            route: manifest.route.clone(),
            candidate_indices: (0..manifest.candidates.len())
                .map(u16::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            generation_defaults_digest: None,
        },
    )?;
    let mut values = serde_json::to_value(binding)?;
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(
        &values
            .as_object_mut()
            .context("binding object")?
            .remove("selection")
            .context("selection")?,
    )?);
    let mut headers = HeaderMap::new();
    for (name, value) in values.as_object().context("binding headers")? {
        headers.insert(
            ::http::HeaderName::from_bytes(name.as_bytes())?,
            value.as_str().context("header text")?.parse()?,
        );
    }
    headers.insert("bitrouter-candidate-selection", encoded.parse()?);
    headers.insert(
        "authorization",
        format!(
            "Bearer {}",
            credential.secret.as_deref().context("issued secret")?
        )
        .parse()?,
    );
    Ok(headers)
}

async fn dispatch(service: &Service, headers: HeaderMap) -> Result<(StatusCode, Value)> {
    let (status, body, _) = dispatch_body(
        service,
        headers,
        json!({
            "model":ROUTE, "messages":[{"role":"user","content":"hello"}]
        }),
    )
    .await?;
    Ok((status, body))
}

async fn dispatch_composed(
    service: &Service,
    headers: HeaderMap,
    selection: &CatalogSelectionRequestV1,
) -> Result<(StatusCode, Value, Option<ExecutionId>)> {
    let mut body = json!({"model":ROUTE,"messages":[{"role":"user","content":"hello"}]});
    body[bitrouter_ai::tenancy::catalog::BODY_MEMBER] = serde_json::to_value(selection)?;
    dispatch_body(service, headers, body).await
}

async fn dispatch_body(
    service: &Service,
    headers: HeaderMap,
    body: Value,
) -> Result<(StatusCode, Value, Option<ExecutionId>)> {
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request =
        Request::post("/v1/chat/completions").body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let execution = response
        .headers()
        .get("x-bitrouter-request-id")
        .map(|value| -> Result<ExecutionId> { Ok(value.to_str()?.try_into()?) })
        .transpose()?;
    let body = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    Ok((status, serde_json::from_slice(&body)?, execution))
}

async fn owned_slot(fixture: &Fixture, operation: &str) -> Result<ResourceRef> {
    let key = ResourceKey {
        owner_kind: OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: operation.try_into()?,
    };
    let applied = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Resource(
                    fixture.tenant.clone(),
                    ResourcePut {
                        format: 1,
                        operation_id: operation.try_into()?,
                        installation_id: fixture.installation.clone(),
                        key: key.clone(),
                        expected_revision: None,
                        definition: Definition::Credential {
                            secret: OWNED_SECRET.into(),
                        },
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id,
        resource_kind: key.resource_kind,
        resource_id: key.resource_id,
        revision_id: applied
            .resource_revision
            .context("owned credential revision")?,
    })
}

async fn receipt(fixture: &Fixture, id: ExecutionId) -> Result<ExecutionReceipt> {
    let body = fixture
        .service
        .read_receipt(
            fixture.operator.clone(),
            fixture.tenant.clone(),
            crate::tenancy::receipts::read::Request::One(id),
        )
        .await
        .map_err(failure)?;
    let receipt: ExecutionReceipt = serde_json::from_slice(&body)?;
    receipt.validate()?;
    Ok(receipt)
}

async fn successful_dispatch(
    fixture: &Fixture,
    credential: &CredentialIssued,
    catalog: &CatalogCandidatesV1,
) -> Result<()> {
    let (status, response) = dispatch(
        &fixture.service,
        headers(fixture, credential, catalog).await?,
    )
    .await?;
    ensure!(
        status == StatusCode::OK,
        "configured dispatch failed: {status} {response}"
    );
    ensure!(response["choices"][0]["message"]["content"] == "shared reply");
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    Ok(())
}

/// Two tenant identities use one configured root cold, after eviction, and after restart.
#[tokio::test]
async fn shared_catalog_dispatch_survives_eviction_and_restart_without_tenant_publication()
-> Result<()> {
    let provider = Provider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let bootstrap = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let credential_a = grant(
        &mut harness.fixture,
        scope(&bootstrap),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "catalog-a",
    )
    .await?;
    let mut second = second_tenant(&harness.fixture).await?;
    let credential_b = grant(
        &mut second,
        scope(&bootstrap),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "catalog-b",
    )
    .await?;
    let tenant_b = second.tenant.clone();
    let first = catalog(
        &harness.fixture,
        &caller(&harness.fixture.service, &credential_a).await?,
        &harness.fixture.tenant,
    )
    .await?;
    let other = catalog(
        &second,
        &caller(&second.service, &credential_b).await?,
        &second.tenant,
    )
    .await?;
    first.validate()?;
    other.validate()?;
    ensure!(first.manifest.tenant_id != other.manifest.tenant_id);
    ensure!(first.manifest.execution_revision == other.manifest.execution_revision);
    ensure!(
        first.manifest.route == other.manifest.route
            && first.manifest.route.owner_kind == OwnerKind::OperatorPool
    );
    ensure!(
        serde_json::to_value(&first.manifest.candidates)?
            == serde_json::to_value(&other.manifest.candidates)?
    );
    let source = first.sources.first().context("configured source")?;
    ensure!(
        source.source_id == SOURCE
            && source.provider == "mock"
            && source.account.is_none()
            && source.model == "native-shared"
            && source.outbound_protocol == ApiProtocol::ChatCompletions
    );
    let encoded = serde_json::to_string(&first)?;
    ensure!(!encoded.contains(SECRET) && !encoded.contains(&provider.url));
    let before = publication_counts(&harness.fixture).await?;
    ensure!(before[..4] == [0, 0, 0, 0]);
    // One cache entry forces A's tenant wrapper out when B uses the shared root.
    successful_dispatch(&harness.fixture, &credential_a, &first).await?;
    successful_dispatch(&second, &credential_b, &other).await?;
    successful_dispatch(&harness.fixture, &credential_a, &first).await?;
    ensure!(provider.calls() == 3);
    ensure!(publication_counts(&harness.fixture).await? == before);
    drop(second);
    harness = harness.restart().await?;
    for (credential, tenant) in [
        (&credential_a, harness.fixture.tenant.clone()),
        (&credential_b, tenant_b),
    ] {
        let read = catalog(
            &harness.fixture,
            &caller(&harness.fixture.service, credential).await?,
            &tenant,
        )
        .await?;
        ensure!(
            read.manifest.tenant_id == tenant
                && read.manifest.route == first.manifest.route
                && read.manifest.execution_revision == first.manifest.execution_revision
        );
        successful_dispatch(&harness.fixture, credential, &read).await?;
    }
    ensure!(provider.calls() == 5);
    ensure!(publication_counts(&harness.fixture).await? == before);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// Catalog descriptors never substitute for exact dependency grants or tenant identity.
#[tokio::test]
async fn catalog_reads_and_dispatch_require_current_tenant_and_dependency_permissions() -> Result<()>
{
    let provider = Provider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let read = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let credential = grant(
        &mut harness.fixture,
        scope(&read),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "grant-full",
    )
    .await?;
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let second = second_tenant(&harness.fixture).await?;
    let foreign = harness
        .fixture
        .service
        .catalog_candidates(
            &authenticated,
            &second.tenant,
            ROUTE,
            GenerationProtocol::ChatCompletions,
        )
        .await;
    ensure!(
        matches!(foreign, Err(error) if matches!(error.0.code, ErrorCode::Forbidden | ErrorCode::NotFound))
    );
    let GrantScope::Resources {
        resources: full_scope,
    } = scope(&read)
    else {
        anyhow::bail!("explicit catalog scope");
    };
    for (index, missing) in full_scope.iter().enumerate() {
        let partial = GrantScope::Resources {
            resources: full_scope
                .iter()
                .filter(|key| *key != missing)
                .cloned()
                .collect(),
        };
        let limited = grant(
            &mut harness.fixture,
            partial,
            TenantRole::TenantAdmin.permissions().to_vec(),
            &format!("missing-{index}"),
        )
        .await?;
        let identity = caller(&harness.fixture.service, &limited).await?;
        ensure!(
            matches!(harness.fixture.service.catalog_candidates(&identity, &harness.fixture.tenant, ROUTE, GenerationProtocol::ChatCompletions).await,
            Err(error) if error.0.code == ErrorCode::Forbidden)
        );
        let (status, _) = dispatch(
            &harness.fixture.service,
            headers(&harness.fixture, &limited, &read).await?,
        )
        .await?;
        ensure!(
            status == StatusCode::FORBIDDEN,
            "missing dependency reached dispatch"
        );
    }
    let reader = grant(
        &mut harness.fixture,
        scope(&read),
        vec![Permission::ResourceRead],
        "catalog-reader",
    )
    .await?;
    catalog(
        &harness.fixture,
        &caller(&harness.fixture.service, &reader).await?,
        &harness.fixture.tenant,
    )
    .await?;
    let (status, _) = dispatch(
        &harness.fixture.service,
        headers(&harness.fixture, &reader, &read).await?,
    )
    .await?;
    ensure!(status == StatusCode::FORBIDDEN && provider.calls() == 0);
    drop(second);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// Revocation after admission prevents the retained configured target from being sent.
#[tokio::test]
async fn catalog_execution_rechecks_revocation_before_provider_send() -> Result<()> {
    use crate::gateway::pipeline::types::{PipelineInput, PipelineRequest};
    let provider = Provider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let read = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let credential = grant(
        &mut harness.fixture,
        scope(&read),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "catalog-executor",
    )
    .await?;
    let bound = headers(&harness.fixture, &credential, &read).await?;
    let submission = harness
        .fixture
        .service
        .submission(&bound)
        .await
        .map_err(failure)?;
    let execution: Arc<TenantExecution> = harness
        .fixture
        .service
        .admit_execution(submission, ROUTE, &ApiProtocol::ChatCompletions)
        .await
        .map_err(failure)?;
    ensure!(provider.calls() == 0);
    result(
        harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                Command::Revoke(
                    harness.fixture.tenant.clone(),
                    credential.credential_id.clone(),
                    Mutation {
                        operation_id: "revoke-admitted-catalog".try_into()?,
                        expected_revision: credential.credential_revision.clone(),
                        installation_id: harness.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        json!({"model":ROUTE,"messages":[{"role":"user","content":"hello"}]}),
    )?;
    let PipelineInput::Generation(prompt) = input else {
        anyhow::bail!("generation input");
    };
    let pipeline = execution.pipeline()?;
    let mut request = PipelineRequest::new(ROUTE, execution.caller(), *prompt);
    request.request_id = execution.id().to_string();
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    execution.attach(pipeline.clone(), &mut request)?;
    ensure!(
        tokio::time::timeout(TIMEOUT, pipeline.execute_detached(request))
            .await?
            .is_err()
    );
    ensure!(
        execution
            .failure()
            .is_some_and(|error| error.0.code == ErrorCode::Unauthorized)
    );
    ensure!(
        provider.calls() == 0,
        "revoked configured execution reached the provider"
    );
    ensure!(harness.fixture.service.submission(&bound).await.is_err());
    drop(execution);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

async fn independent_pool_dispatch(
    fixture: &mut Fixture,
    url: &str,
    profile: bitrouter_ai::tenancy::admission::AdmissionProfile,
) -> Result<()> {
    async fn put(
        fixture: &Fixture,
        kind: ResourceKind,
        id: &str,
        definition: Definition,
    ) -> Result<ResourceRef> {
        let key = ResourceKey {
            owner_kind: OwnerKind::OperatorPool,
            owner_id: "independent".try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
        };
        let applied = result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Resource(
                        fixture.tenant.clone(),
                        ResourcePut {
                            format: 1,
                            operation_id: id.try_into()?,
                            installation_id: fixture.installation.clone(),
                            key: key.clone(),
                            expected_revision: None,
                            definition,
                        },
                    ),
                )
                .await
                .map_err(failure)
                .with_context(|| format!("provision independent resource {id}"))?,
        )?;
        Ok(ResourceRef {
            owner_kind: key.owner_kind,
            owner_id: key.owner_id,
            resource_kind: key.resource_kind,
            resource_id: key.resource_id,
            revision_id: applied
                .resource_revision
                .context("applied generic resource")?,
        })
    }
    let credential = put(
        fixture,
        ResourceKind::Credential,
        "independent-credential",
        Definition::Credential {
            secret: SECRET.into(),
        },
    )
    .await?;
    let profile = put(
        fixture,
        ResourceKind::Policy,
        "independent-dispatch-profile",
        Definition::AdmissionProfile { profile },
    )
    .await?;
    let route = put(fixture, ResourceKind::Route, "independent-route", Definition::ProfiledRoute {
        routing: json!({"providers":{"mock":{"api_base":url,"api_protocol":[{"*":["chat_completions","messages","responses"]}],"models":[{"id":"native-shared"}]}}}),
        selector: "mock:native-shared".into(), policy: None,
        credentials: vec![bitrouter_ai::tenancy::resources::CredentialBinding { provider: "mock".into(), account: "default".into(), credential: credential.clone() }],
        attachments: [GenerationProtocol::ChatCompletions, GenerationProtocol::Messages, GenerationProtocol::Responses].into_iter().map(|inbound_protocol| bitrouter_ai::tenancy::admission::AdmissionAttachment {
            inbound_protocol, candidate_index: 0, credential: credential.clone(), profile: profile.clone(),
        }).collect(),
    }).await?;
    let stored = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(
                fixture.tenant.clone(),
                bitrouter_ai::tenancy::configuration::ConfigurationPut {
                    operation_id: "independent-configuration".try_into()?,
                    installation_id: fixture.installation.clone(),
                    expected_authority_revision: fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .revision,
                    document: bitrouter_ai::tenancy::configuration::Configuration {
                        routes: vec![route.clone()],
                        policy: None,
                        eval_publication: false,
                        generation_defaults:
                            bitrouter_ai::tenancy::configuration::GenerationDefaults {
                                max_output_tokens: 128,
                                temperature: 0.5,
                            },
                        limits: bitrouter_ai::tenancy::configuration::Limits {
                            active_executions: 1,
                            queued_work: 1,
                        },
                    },
                },
            ),
        )
        .await
        .map_err(failure)
        .context("create independent configuration")?;
    let StoredResponse::Configuration(retained) = stored else {
        anyhow::bail!("expected independent retained configuration");
    };
    let revision = retained.revision_id;
    let manifest = fixture
        .service
        .candidate_manifest(
            &fixture.operator,
            &fixture.tenant,
            &revision,
            &route,
            GenerationProtocol::ChatCompletions,
        )
        .await
        .map_err(failure)
        .context("read independent candidate manifest")?;
    let issued = grant(
        fixture,
        GrantScope::Tenant {
            operator_pools: vec![route.key(), credential.key(), profile.key()],
        },
        TenantRole::TenantAdmin.permissions().to_vec(),
        "independent-dispatch-grant",
    )
    .await
    .context("grant independent resource keys")?;
    let bound = manifest_headers(fixture, &issued, &manifest).await?;
    let (status, body, execution) = dispatch_body(
        &fixture.service,
        bound,
        json!({"model":route.resource_id,"messages":[{"role":"user","content":"generic route"}]}),
    )
    .await?;
    ensure!(
        status == StatusCode::OK,
        "independent pool dispatch failed: {body}"
    );
    let receipt = receipt(fixture, execution.context("generic execution identity")?).await?;
    ensure!(
        receipt.outcome == Outcome::Completed
            && receipt.attempts.len() == 1
            && accepted_selection(&receipt.dispatch)?.route == route,
        "independent generic dispatch must retain its actual route and attempt"
    );
    Ok(())
}

/// Installation authority cannot replace file-owned credentials or revoke catalog objects.
#[tokio::test]
async fn file_owned_platform_resources_refuse_management_writes() -> Result<()> {
    let provider = Provider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let read = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let credential = grant(
        &mut harness.fixture,
        scope(&read),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "catalog-admin",
    )
    .await?;
    let tenant_caller = caller(&harness.fixture.service, &credential).await?;
    let slot = &read
        .manifest
        .candidates
        .first()
        .context("configured candidate")?
        .credential;
    for (index, identity) in [harness.fixture.operator.clone(), tenant_caller]
        .into_iter()
        .enumerate()
    {
        for (action, expected_revision) in
            [("create", None), ("rotate", Some(slot.revision_id.clone()))]
        {
            let response = harness
                .fixture
                .service
                .execute(
                    identity.clone(),
                    Command::Resource(
                        harness.fixture.tenant.clone(),
                        ResourcePut {
                            format: 1,
                            operation_id: format!("file-{action}-{index}").try_into()?,
                            installation_id: harness.fixture.installation.clone(),
                            key: slot.key(),
                            expected_revision,
                            definition: Definition::Credential {
                                secret: "replacement-upstream-secret".into(),
                            },
                        },
                    ),
                )
                .await;
            forbidden(response)?;
        }
        for (ordinal, reference) in std::iter::once(&read.manifest.route)
            .chain(std::iter::once(slot))
            .chain(read.manifest.candidates[0].admission_profile.iter())
            .enumerate()
        {
            let response = harness
                .fixture
                .service
                .execute(
                    identity.clone(),
                    Command::RevokeResource(
                        harness.fixture.tenant.clone(),
                        ResourceRevoke {
                            format: 1,
                            operation_id: format!("file-revoke-{index}-{ordinal}").try_into()?,
                            installation_id: harness.fixture.installation.clone(),
                            reference: reference.clone(),
                            expected_authority_revision: harness
                                .fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .revision,
                        },
                    ),
                )
                .await;
            forbidden(response)?;
        }
    }
    let after = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    ensure!(
        after.manifest.execution_revision == read.manifest.execution_revision
            && after.manifest.route == read.manifest.route
    );
    successful_dispatch(&harness.fixture, &credential, &after).await?;
    ensure!(provider.calls() == 1);
    // The refusal must preserve ordinary tenant-owned credential management.
    result(
        harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                Command::Resource(
                    harness.fixture.tenant.clone(),
                    ResourcePut {
                        format: 1,
                        operation_id: "owned-credential-remains-writable".try_into()?,
                        installation_id: harness.fixture.installation.clone(),
                        key: ResourceKey {
                            owner_kind: OwnerKind::Tenant,
                            owner_id: harness.fixture.tenant.as_str().try_into()?,
                            resource_kind: ResourceKind::Credential,
                            resource_id: "owned-slot".try_into()?,
                        },
                        expected_revision: None,
                        definition: Definition::Credential {
                            secret: "tenant-owned-secret".into(),
                        },
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    // File ownership fences its exact pool, not unrelated generic operator resources.
    let profile = harness
        .config
        .tenancy
        .catalog
        .as_ref()
        .context("catalog declaration")?
        .profiles
        .get(SOURCE)
        .context("configured profile")?
        .clone();
    for pool in ["platform", "independent"] {
        let response = harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                Command::Resource(
                    harness.fixture.tenant.clone(),
                    ResourcePut {
                        format: 1,
                        operation_id: format!("profile-namespace-{pool}").try_into()?,
                        installation_id: harness.fixture.installation.clone(),
                        key: ResourceKey {
                            owner_kind: OwnerKind::OperatorPool,
                            owner_id: pool.try_into()?,
                            resource_kind: ResourceKind::Policy,
                            resource_id: "generic-profile".try_into()?,
                        },
                        expected_revision: None,
                        definition: Definition::AdmissionProfile {
                            profile: profile.clone(),
                        },
                    },
                ),
            )
            .await;
        if pool == "platform" {
            forbidden(response)?;
        } else {
            result(response.map_err(failure)?)?;
        }
    }
    let generic_profile = harness
        .config
        .tenancy
        .catalog
        .as_ref()
        .context("catalog")?
        .profiles
        .get(SOURCE)
        .context("profile")?
        .clone();
    independent_pool_dispatch(&mut harness.fixture, &provider.url, generic_profile).await?;
    ensure!(
        provider.calls() == 2,
        "independent pool must actually dispatch once"
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// One bound mixed order falls back through actual sends and retains one tenant receipt.
#[tokio::test]
async fn mixed_catalog_selection_binds_order_and_keeps_one_execution_owner() -> Result<()> {
    let mut provider = Provider::with_owned_rejection(true).await?;
    let mut harness = Harness::new(&provider.url).await?;
    // A mixed invocation owns two credential slots; eviction is tested separately.
    harness.fixture.service.caches = Arc::new(Caches::with_limits(
        1,
        128 * 1024 * 1024,
        4,
        8 * 1024 * 1024,
    )?);
    let base = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let template = base
        .templates
        .first()
        .context("configured mixed template")?;
    ensure!(template.source_id == SOURCE && template.credential_ref.is_some());
    let slot = owned_slot(&harness.fixture, "mixed-owned-a").await?;
    let second = second_tenant(&harness.fixture).await?;
    let foreign_slot = owned_slot(&second, "mixed-owned-b").await?;
    let selection = CatalogSelectionRequestV1 {
        schema_version: 1,
        catalog_revision: base.manifest.execution_revision.clone(),
        route_id: base.manifest.route.resource_id.clone(),
        inbound_protocol: GenerationProtocol::ChatCompletions,
        candidates: vec![
            CatalogCandidateV1 {
                template_id: template.template_id.clone(),
                credential_ref: Some(slot.clone()),
                route_ref: None,
            },
            CatalogCandidateV1 {
                template_id: template.template_id.clone(),
                credential_ref: None,
                route_ref: None,
            },
        ],
    };
    let bootstrap = harness
        .fixture
        .service
        .prepare_catalog_selection(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            &selection,
        )
        .await
        .map_err(failure)?;
    let credential = grant(
        &mut harness.fixture,
        scope(&bootstrap.candidates),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "mixed-executor",
    )
    .await?;
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let before = publication_counts(&harness.fixture).await?;
    let prepared = harness
        .fixture
        .service
        .prepare_catalog_selection(&authenticated, &harness.fixture.tenant, &selection)
        .await
        .map_err(failure)?;
    prepared.validate()?;
    ensure!(prepared.selection == selection && prepared.candidates.manifest.candidates.len() == 2);
    ensure!(prepared.candidates.manifest.candidates[0].credential == slot);
    ensure!(
        prepared.candidates.manifest.candidates[1].credential
            == *template.credential_ref.as_ref().context("platform slot")?
    );
    ensure!(prepared.candidates.manifest.route.revision_id != base.manifest.route.revision_id);
    let public = serde_json::to_string(&prepared)?;
    ensure!(
        !public.contains(SECRET)
            && !public.contains(OWNED_SECRET)
            && !public.contains(&provider.url)
    );

    ensure!(matches!(harness.fixture.service.prepare_catalog_selection(
        &authenticated, &second.tenant, &selection,
    ).await, Err(error) if error.0.code == ErrorCode::Forbidden));
    let mut unknown = selection.clone();
    unknown.candidates[0].template_id = "unknown-configured-template".try_into()?;
    let mut foreign = selection.clone();
    foreign.candidates[0].credential_ref = Some(foreign_slot);
    for (invalid, expected) in [
        (unknown, ErrorCode::InvalidRequest),
        (foreign, ErrorCode::Forbidden),
    ] {
        ensure!(matches!(harness.fixture.service.prepare_catalog_selection(
            &authenticated, &harness.fixture.tenant, &invalid,
        ).await, Err(error) if error.0.code == expected));
    }
    // The same bound headers cannot be reused with a different final funding order.
    let mut reordered = selection.clone();
    reordered.candidates.reverse();
    let (status, error, _) = dispatch_composed(
        &harness.fixture.service,
        headers(&harness.fixture, &credential, &prepared.candidates).await?,
        &reordered,
    )
    .await?;
    ensure!(status == StatusCode::CONFLICT && error["code"] == "capability_mismatch");
    let mut reordered_headers =
        headers(&harness.fixture, &credential, &prepared.candidates).await?;
    let encoded = reordered_headers
        .get("bitrouter-candidate-selection")
        .context("selection header")?
        .to_str()?;
    let mut ordinals: OrderedCandidateSelection =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?;
    ordinals.candidate_indices.reverse();
    reordered_headers.insert(
        "bitrouter-candidate-selection",
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&ordinals)?)
            .parse()?,
    );
    let (status, error, _) =
        dispatch_composed(&harness.fixture.service, reordered_headers, &selection).await?;
    ensure!(status == StatusCode::CONFLICT && error["code"] == "capability_mismatch");
    ensure!(
        provider.calls() == 0,
        "invalid tenant/template/order reached the provider"
    );
    ensure!(publication_counts(&harness.fixture).await? == before);

    let (status, response, execution) = dispatch_composed(
        &harness.fixture.service,
        headers(&harness.fixture, &credential, &prepared.candidates).await?,
        &selection,
    )
    .await?;
    ensure!(
        status == StatusCode::OK,
        "mixed dispatch failed: {status} {response}"
    );
    ensure!(response["choices"][0]["message"]["content"] == "shared reply");
    provider.observed(OWNED_SECRET).await?;
    provider.observed(SECRET).await?;
    ensure!(provider.calls() == 2);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let execution = execution.context("mixed execution response identity")?;
    let receipt = receipt(&harness.fixture, execution.clone()).await?;
    ensure!(
        receipt.execution_id == execution
            && receipt.tenant_id == harness.fixture.tenant
            && receipt.principal_id == harness.fixture.principal
            && receipt.outcome == Outcome::Completed
    );
    ensure!(
        receipt.execution_revision == prepared.candidates.manifest.execution_revision
            && accepted_selection(&receipt.dispatch)?.route == prepared.candidates.manifest.route
            && accepted_selection(&receipt.dispatch)?.candidate_indices == [0, 1]
    );
    ensure!(
        receipt.attempts.len() == 2
            && receipt.attempts[0].outcome == Outcome::Failed
            && receipt.attempts[0].observed_http_status == Some(503)
            && receipt.attempts[1].outcome == Outcome::Completed
            && receipt.attempts[1].observed_http_status == Some(200)
    );
    for (attempt, candidate) in receipt
        .attempts
        .iter()
        .zip(&prepared.candidates.manifest.candidates)
    {
        ensure!(
            attempt.candidate_index == Some(candidate.candidate_index)
                && attempt.target.as_ref().is_some_and(
                    |target| target.credential_version == candidate.credential.revision_id
                )
        );
    }
    let row = harness
        .fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM tenancy_execution_receipts",
        ))
        .await?
        .context("mixed receipt owner count")?;
    ensure!(
        row.try_get::<i64>("", "n")? == 1,
        "fallback created another execution/receipt owner"
    );
    let encoded = serde_json::to_string(&receipt)?;
    ensure!(
        !encoded.contains(SECRET)
            && !encoded.contains(OWNED_SECRET)
            && !encoded.contains(&provider.url)
    );
    ensure!(publication_counts(&harness.fixture).await? == before);
    drop(second);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// Missing own keys and revisions are request failures even without an endpoint override.
#[tokio::test]
async fn nonexistent_byok_keys_and_revisions_do_not_disable_valid_tenant_sends() -> Result<()> {
    let mut provider = Provider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let base = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let slot = owned_slot(&harness.fixture, "existing-byok-slot").await?;
    let selection = CatalogSelectionRequestV1 {
        schema_version: 1,
        catalog_revision: base.manifest.execution_revision.clone(),
        route_id: base.manifest.route.resource_id.clone(),
        inbound_protocol: GenerationProtocol::ChatCompletions,
        candidates: vec![CatalogCandidateV1 {
            template_id: base
                .templates
                .first()
                .context("BYOK template")?
                .template_id
                .clone(),
            credential_ref: Some(slot.clone()),
            route_ref: None,
        }],
    };
    let bootstrap = harness
        .fixture
        .service
        .prepare_catalog_selection(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            &selection,
        )
        .await
        .map_err(failure)?;
    let GrantScope::Resources { resources } = scope(&bootstrap.candidates) else {
        anyhow::bail!("configured BYOK dependency scope");
    };
    // Whole-tenant authority permits both nonexistent keys and arbitrary
    // revisions under the real key. Refusal must authenticate the artifact.
    let credential = grant(
        &mut harness.fixture,
        GrantScope::Tenant {
            operator_pools: resources
                .into_iter()
                .filter(|key| key.owner_kind == OwnerKind::OperatorPool)
                .collect(),
        },
        TenantRole::TenantAdmin.permissions().to_vec(),
        "missing-byok-executor",
    )
    .await?;
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let mut second = second_tenant(&harness.fixture).await?;
    let second_descriptor = catalog(&second, &second.operator, &second.tenant).await?;
    let second_credential = grant(
        &mut second,
        scope(&second_descriptor),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "unaffected-tenant-executor",
    )
    .await?;
    let before = publication_counts(&harness.fixture).await?;
    let mut missing_key = slot.clone();
    missing_key.resource_id = "nonexistent-owned-key".try_into()?;
    missing_key.revision_id = "nonexistent-owned-key-revision".try_into()?;
    let mut missing_revision = slot.clone();
    missing_revision.revision_id = "nonexistent-existing-key-revision".try_into()?;
    let mut expected_calls = 0;
    for reference in [missing_key, missing_revision] {
        let mut invalid = selection.clone();
        invalid.candidates[0].credential_ref = Some(reference);
        invalid.validate()?;
        let refusal = harness
            .fixture
            .service
            .prepare_catalog_selection(&authenticated, &harness.fixture.tenant, &invalid)
            .await
            .err()
            .context("nonexistent BYOK artifact was prepared")?;
        ensure!(
            refusal.0.code == ErrorCode::InvalidRequest,
            "missing artifact became a service/authority failure: {:?}",
            refusal.0.code
        );
        ensure!(
            provider.calls() == expected_calls,
            "refused credential reached provider"
        );

        let prepared = harness
            .fixture
            .service
            .prepare_catalog_selection(&authenticated, &harness.fixture.tenant, &selection)
            .await
            .map_err(failure)?;
        let (status, response, execution) = dispatch_composed(
            &harness.fixture.service,
            headers(&harness.fixture, &credential, &prepared.candidates).await?,
            &selection,
        )
        .await?;
        ensure!(
            status == StatusCode::OK,
            "valid own-key dispatch failed: {status} {response}"
        );
        provider.observed(OWNED_SECRET).await?;
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        let receipt = receipt(&harness.fixture, execution.context("valid BYOK execution")?).await?;
        ensure!(
            receipt.tenant_id == harness.fixture.tenant
                && receipt.outcome == Outcome::Completed
                && receipt.execution_revision == prepared.candidates.manifest.execution_revision
                && receipt.attempts.len() == 1
                && receipt.attempts[0]
                    .target
                    .as_ref()
                    .is_some_and(|target| target.credential_version == slot.revision_id)
        );
        successful_dispatch(&second, &second_credential, &second_descriptor).await?;
        provider.observed(SECRET).await?;
        expected_calls += 2;
        ensure!(provider.calls() == expected_calls);
    }
    ensure!(publication_counts(&harness.fixture).await? == before);
    drop(second);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// A configured template without a platform key remains usable through an owned credential.
#[tokio::test]
async fn byok_only_catalog_descriptor_executes_without_a_platform_key() -> Result<()> {
    let mut provider = Provider::start().await?;
    let mut harness = Harness::with_platform_secret(&provider.url, "").await?;
    let base = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    base.validate()?;
    ensure!(base.manifest.candidates.is_empty() && base.sources.is_empty());
    ensure!(base.templates.len() == 1);
    let template = base
        .templates
        .first()
        .context("BYOK-only configured template")?;
    ensure!(
        template.source_id == SOURCE
            && template.provider == "mock"
            && template.account.is_none()
            && template.model == "native-shared"
            && template.outbound_protocol == ApiProtocol::ChatCompletions
            && template.credential_ref.is_none()
    );
    let slot = owned_slot(&harness.fixture, "byok-only-slot").await?;
    let selection = CatalogSelectionRequestV1 {
        schema_version: 1,
        catalog_revision: base.manifest.execution_revision.clone(),
        route_id: base.manifest.route.resource_id.clone(),
        inbound_protocol: GenerationProtocol::ChatCompletions,
        candidates: vec![CatalogCandidateV1 {
            template_id: template.template_id.clone(),
            credential_ref: Some(slot.clone()),
            route_ref: None,
        }],
    };
    let bootstrap = harness
        .fixture
        .service
        .prepare_catalog_selection(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            &selection,
        )
        .await
        .map_err(failure)?;
    let credential = grant(
        &mut harness.fixture,
        scope(&bootstrap.candidates),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "byok-only-executor",
    )
    .await?;
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let before = publication_counts(&harness.fixture).await?;
    let read = catalog(&harness.fixture, &authenticated, &harness.fixture.tenant).await?;
    ensure!(read.manifest.candidates.is_empty() && read.templates[0].credential_ref.is_none());
    let mut anonymous = selection.clone();
    anonymous.candidates[0].credential_ref = None;
    ensure!(matches!(harness.fixture.service.prepare_catalog_selection(
        &authenticated, &harness.fixture.tenant, &anonymous,
    ).await, Err(error) if error.0.code == ErrorCode::InvalidRequest));
    let prepared = harness
        .fixture
        .service
        .prepare_catalog_selection(&authenticated, &harness.fixture.tenant, &selection)
        .await
        .map_err(failure)?;
    ensure!(prepared.candidates.manifest.candidates[0].credential == slot);
    let encoded = serde_json::to_string(&prepared)?;
    ensure!(!encoded.contains(OWNED_SECRET) && !encoded.contains(&provider.url));
    let (status, response, execution) = dispatch_composed(
        &harness.fixture.service,
        headers(&harness.fixture, &credential, &prepared.candidates).await?,
        &selection,
    )
    .await?;
    ensure!(
        status == StatusCode::OK,
        "BYOK-only dispatch failed: {status} {response}"
    );
    provider.observed(OWNED_SECRET).await?;
    ensure!(provider.calls() == 1);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let receipt = receipt(
        &harness.fixture,
        execution.context("BYOK-only execution identity")?,
    )
    .await?;
    ensure!(
        receipt.tenant_id == harness.fixture.tenant
            && receipt.principal_id == harness.fixture.principal
            && receipt.outcome == Outcome::Completed
            && receipt.attempts.len() == 1
            && receipt.attempts[0].candidate_index == Some(0)
            && receipt.attempts[0]
                .target
                .as_ref()
                .is_some_and(|target| target.credential_version == slot.revision_id)
    );
    ensure!(publication_counts(&harness.fixture).await? == before);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// The same owned key follows its selected immutable custom base and live route revocation.
#[tokio::test]
async fn owned_catalog_route_selects_custom_base_without_retargeting_the_credential() -> Result<()>
{
    let mut default_provider = Provider::start().await?;
    let mut custom_provider = Provider::start().await?;
    let mut harness = Harness::new(&default_provider.url).await?;
    let base = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let template = base
        .templates
        .first()
        .context("configured custom-base template")?;
    let slot = owned_slot(&harness.fixture, "custom-base-slot").await?;
    let route_key = ResourceKey {
        owner_kind: OwnerKind::Tenant,
        owner_id: harness.fixture.tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "owned-custom-base".try_into()?,
    };
    let created = result(
        harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                Command::Resource(
                    harness.fixture.tenant.clone(),
                    ResourcePut {
                        format: 1,
                        operation_id: "create-owned-custom-base".try_into()?,
                        installation_id: harness.fixture.installation.clone(),
                        key: route_key.clone(),
                        expected_revision: None,
                        definition: Definition::Route {
                            routing: json!({"providers":{"mock":{"api_base":custom_provider.url,
                    "api_protocol":[{"*":"chat_completions"}],"models":[{"id":"native-shared"}]}}}),
                            selector: "mock:native-shared".into(),
                            policy: None,
                            credentials: vec![
                                bitrouter_ai::tenancy::resources::CredentialBinding {
                                    provider: "mock".into(),
                                    account: "default".into(),
                                    credential: slot.clone(),
                                },
                            ],
                        },
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let route = ResourceRef {
        owner_kind: route_key.owner_kind,
        owner_id: route_key.owner_id,
        resource_kind: route_key.resource_kind,
        resource_id: route_key.resource_id,
        revision_id: created
            .resource_revision
            .context("owned custom-base route revision")?,
    };
    let default_selection = CatalogSelectionRequestV1 {
        schema_version: 1,
        catalog_revision: base.manifest.execution_revision.clone(),
        route_id: base.manifest.route.resource_id.clone(),
        inbound_protocol: GenerationProtocol::ChatCompletions,
        candidates: vec![CatalogCandidateV1 {
            template_id: template.template_id.clone(),
            credential_ref: Some(slot.clone()),
            route_ref: None,
        }],
    };
    let mut custom_selection = default_selection.clone();
    custom_selection.candidates[0].route_ref = Some(route.clone());
    let bootstrap = harness
        .fixture
        .service
        .prepare_catalog_selection(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            &custom_selection,
        )
        .await
        .map_err(failure)?;
    let mut grants = scope(&bootstrap.candidates);
    let GrantScope::Resources { resources } = &mut grants else {
        anyhow::bail!("custom route exact scope");
    };
    resources.push(route.key());
    let credential = grant(
        &mut harness.fixture,
        grants,
        TenantRole::TenantAdmin.permissions().to_vec(),
        "custom-base-executor",
    )
    .await?;
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let before = publication_counts(&harness.fixture).await?;
    let default = harness
        .fixture
        .service
        .prepare_catalog_selection(&authenticated, &harness.fixture.tenant, &default_selection)
        .await
        .map_err(failure)?;
    let custom = harness
        .fixture
        .service
        .prepare_catalog_selection(&authenticated, &harness.fixture.tenant, &custom_selection)
        .await
        .map_err(failure)?;
    ensure!(
        default.candidates.manifest.execution_revision
            != custom.candidates.manifest.execution_revision,
        "custom base was omitted from composition identity"
    );
    ensure!(
        default.candidates.manifest.candidates[0].credential == slot
            && custom.candidates.manifest.candidates[0].credential == slot
    );
    let mut platform_override = custom_selection.clone();
    platform_override.candidates[0].credential_ref = None;
    ensure!(matches!(harness.fixture.service.prepare_catalog_selection(
        &authenticated, &harness.fixture.tenant, &platform_override,
    ).await, Err(error) if error.0.code == ErrorCode::InvalidRequest));
    ensure!(default_provider.calls() == 0 && custom_provider.calls() == 0);

    for prepared in [&default, &custom] {
        let encoded = serde_json::to_string(prepared)?;
        ensure!(
            !encoded.contains(OWNED_SECRET)
                && !encoded.contains(&default_provider.url)
                && !encoded.contains(&custom_provider.url)
        );
        let (status, response, execution) = dispatch_composed(
            &harness.fixture.service,
            headers(&harness.fixture, &credential, &prepared.candidates).await?,
            &prepared.selection,
        )
        .await?;
        ensure!(
            status == StatusCode::OK,
            "owned target dispatch failed: {status} {response}"
        );
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        let receipt = receipt(
            &harness.fixture,
            execution.context("owned target execution identity")?,
        )
        .await?;
        ensure!(
            receipt.tenant_id == harness.fixture.tenant
                && receipt.principal_id == harness.fixture.principal
                && receipt.execution_revision == prepared.candidates.manifest.execution_revision
                && accepted_selection(&receipt.dispatch)?.route
                    == prepared.candidates.manifest.route
                && receipt.attempts.len() == 1
                && receipt.outcome == Outcome::Completed
                && receipt.attempts[0]
                    .target
                    .as_ref()
                    .is_some_and(|target| target.credential_version == slot.revision_id)
        );
    }
    default_provider.observed(OWNED_SECRET).await?;
    custom_provider.observed(OWNED_SECRET).await?;
    ensure!(
        default_provider.calls() == 1 && custom_provider.calls() == 1,
        "owned custom base silently sent the key to the template endpoint"
    );
    ensure!(publication_counts(&harness.fixture).await? == before);
    // The second dispatch warmed its exact custom route. Its retained cache is not authority.
    result(
        harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                Command::RevokeResource(
                    harness.fixture.tenant.clone(),
                    ResourceRevoke {
                        format: 1,
                        operation_id: "revoke-warm-custom-base".try_into()?,
                        installation_id: harness.fixture.installation.clone(),
                        reference: route,
                        expected_authority_revision: harness
                            .fixture
                            .service
                            .current()
                            .map_err(failure)?
                            .revision,
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let (status, error, _) = dispatch_composed(
        &harness.fixture.service,
        headers(&harness.fixture, &credential, &custom.candidates).await?,
        &custom.selection,
    )
    .await?;
    ensure!(status == StatusCode::FORBIDDEN && error["code"] == "forbidden");
    ensure!(
        default_provider.calls() == 1 && custom_provider.calls() == 1,
        "revoked custom route reached a provider"
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

fn forbidden(
    response: std::result::Result<StoredResponse, crate::tenancy::error::Failure>,
) -> Result<()> {
    match response {
        Err(error) => ensure!(error.0.code == ErrorCode::Forbidden),
        Ok(StoredResponse::Failure(error)) => ensure!(error.code == ErrorCode::Forbidden),
        _ => anyhow::bail!("file-owned resource accepted a management mutation"),
    }
    Ok(())
}
