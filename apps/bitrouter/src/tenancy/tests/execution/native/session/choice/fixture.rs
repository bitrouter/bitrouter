use super::*;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    identity::RevisionId,
};
use sea_orm::DatabaseConnection;

pub(in crate::tenancy::tests::execution::native) const CHILD: &str = "retained-context-choice";

pub(in crate::tenancy::tests::execution::native) struct Bound {
    pub fixture: Fixture,
    pub route: ResourceRef,
    pub headers: HeaderMap,
    pub key: CredentialIssued,
    pub revision: RevisionId,
}

pub(super) async fn sqlite(
    generation: &Provider,
    choice: &wire::Endpoint,
    directory: &std::path::Path,
    capacity: u16,
) -> Result<Bound> {
    let url = format!("sqlite://{}", directory.join("owned.db").display());
    let lease = crate::tenancy::ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &lease).await?;
    prepare(db, Some(lease), generation, choice, capacity, &mut |_| {}).await
}

pub(super) async fn prepare(
    db: DatabaseConnection,
    lease: Option<crate::tenancy::ownership::Lease>,
    generation: &Provider,
    choice: &wire::Endpoint,
    capacity: u16,
    started: &mut dyn FnMut(&Service),
) -> Result<Bound> {
    let mut fixture = Fixture::with_database(db, OperationLimits::default(), started).await?;
    fixture.service.shutdown().await?;
    let resources = Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?);
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "choice-r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let child = reference(ResourceKind::Route, CHILD)?;
    let generation_key = reference(ResourceKind::Credential, "generation-key")?;
    let choice_key = reference(ResourceKind::Credential, "choice-key")?;
    let route_definition = |url: &str, protocol: &str, model: &str, credential: &ResourceRef| {
        json!({
            "kind":"route", "selector":"bitrouter/owned", "policy":null,
            "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":protocol}],"models":[{"id":model}]}},
                "routers":{"owned":{"selection":{"kind":"model","model":format!("mock:{model}")}}}},
            "credentials":[{"provider":"mock","account":"default","credential":credential}]
        })
    };
    let mut parent = route_definition(
        &generation.url,
        "chat_completions",
        "model-a",
        &generation_key,
    );
    // These small replies exercise permission and evidence preservation with
    // any positive byte saving; the production default switching margin is 10%.
    parent["core_choice"] = json!({"profile":"choice_v1","route":child,
        "policy":{"retain_recent_groups":0,"minimum_savings_fraction":0.0}});
    // Parent precedes its dependency: the real import transaction must validate
    // the complete exact graph, independently of JSON ordering.
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1,"installation_id":fixture.installation,"resources":[
            {"reference":route,"definition":parent},
            {"reference":generation_key,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
            {"reference":child,"definition":route_definition(&choice.url,"decisions","judge-r1",&choice_key)},
            {"reference":choice_key,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-choice-secret"}}}}
        ]
    }))?).await?;
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        lease,
        OperationLimits::default(),
        snapshot,
        resources,
    )
    .await?;
    started(&fixture.service);
    let mut config = fixture.configuration_input("choice-config")?;
    config.document.routes = vec![route.clone()];
    config.document.limits.active_executions = capacity;
    config.document.limits.queued_work = 4;
    config.document.generation_defaults.max_output_tokens = 25;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), config),
        )
        .await
        .map_err(failure)?
    {
        crate::tenancy::store::StoredResponse::Configuration(value) => value.revision_id,
        _ => anyhow::bail!("Choice configuration reply"),
    };
    let key = fixture.issue("choice-executor").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = super::super::super::super::headers(
        &fixture,
        key.secret.as_deref().context("Choice credential")?,
        &revision,
    )?;
    Ok(Bound {
        fixture,
        route,
        headers,
        key,
        revision,
    })
}

pub(super) fn task() -> TaskInput {
    let mut task = input();
    // The current planner emits its three real routing Choice questions even
    // when the first turn has no optional historical evidence yet.
    task.routing.model = bitrouter_ai::core::protocol::ModelMode::Policy;
    task
}

pub(in crate::tenancy::tests::execution::native) async fn open(
    bound: &Bound,
    generation: &Provider,
    workspace: &std::path::Path,
    spies: Arc<HostSpies>,
) -> Result<NativeSession> {
    let app = Arc::new(
        host_app(&bound.fixture, &generation.url, spies)?
            .with_tenancy(bound.fixture.service.clone()),
    );
    Ok(NativeSession::open_authenticated(
        app,
        bound.headers.clone(),
        "choice-session",
        workspace,
        ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await?)
}

pub(super) async fn journal(bound: &Bound) -> Result<Value> {
    let row = bound
        .fixture
        .db
        .query_one(Statement::from_string(
            bound.fixture.db.get_database_backend(),
            "SELECT payload FROM bro_managed_sessions",
        ))
        .await?
        .context("owned native row")?;
    let value: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
    Ok(value["journal"].clone())
}
