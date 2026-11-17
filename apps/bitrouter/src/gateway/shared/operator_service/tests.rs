use crate::runtime_crypto::DeploymentKey;
use anyhow::{Context, Result, ensure};
use axum::{body::Body, http::Method};
use bitrouter_ai::gateway::operator::{
    Command, CredentialBinding, CredentialImport, CredentialInput, CredentialMetadata, Grant,
    Mutation, OperationLookup, Outcome, Publication, PublicationPut, Receipt, ResourcePut,
    Revision,
};
use sea_orm::{ConnectionTrait as _, DatabaseBackend, Statement};
use serde_json::json;
use tower::ServiceExt;

use super::*;
use crate::config::shared_process::operator::{Config, Credential};
use crate::gateway::shared::{
    publication::Configuration,
    schema,
    store::{self, Store},
};

const WRITE_TOKEN: &str = "fixture-write-token-0123456789abcdef";
const READ_TOKEN: &str = "fixture-read-token-0123456789abcdef";

fn authenticator(writer_grants: Vec<Grant>) -> Result<Arc<auth::Auth>> {
    let config = Config {
        listen: "127.0.0.1:4358".parse()?,
        max_connections: 16,
        credentials: vec![
            Credential {
                id: "writer".into(),
                token_env: "WRITE".into(),
                grants: writer_grants,
            },
            Credential {
                id: "reader".into(),
                token_env: "READ".into(),
                grants: vec![Grant::Read],
            },
        ],
    };
    Ok(Arc::new(auth::Auth::from_lookup(
        &config,
        |name| match name {
            "WRITE" => Some(WRITE_TOKEN.into()),
            "READ" => Some(READ_TOKEN.into()),
            _ => None,
        },
    )?))
}

fn input(id: &str) -> Mutation {
    Mutation {
        operation_id: id.into(),
        command: Command::PutResource(ResourcePut {
            resource_pool_id: format!("pool/{id} 東京%&+?"),
            expected_revision: None,
            concurrent_attempts: 3,
        }),
    }
}

fn post(input: &Mutation) -> Result<Request> {
    Ok(Request::builder()
        .method(Method::POST)
        .uri("/operator/v1/mutations")
        .header(header::AUTHORIZATION, format!("Bearer {WRITE_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(input)?))?)
}

fn get(path: &str, query: &[(&str, &str)], token: &str) -> Result<Request> {
    let mut url = url::Url::parse(&format!("http://localhost/operator/v1/{path}"))?;
    url.query_pairs_mut().extend_pairs(query.iter().copied());
    Ok(Request::builder()
        .uri(format!(
            "{}?{}",
            url.path(),
            url.query().unwrap_or_default()
        ))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())?)
}

async fn decode<T: serde::de::DeserializeOwned>(
    response: Response,
    status: StatusCode,
) -> Result<T> {
    ensure!(
        response.status() == status,
        "unexpected operator status {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .is_some_and(|value| value == "no-store")
    );
    Ok(serde_json::from_slice(
        &to_bytes(response.into_body(), 2 * 1024 * 1024).await?,
    )?)
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
async fn postgres_operator_http_authorization_receipts_and_deadlines() -> Result<()> {
    let database = store::connect(
        &std::env::var("BITROUTER_TEST_DATABASE_URL")?,
        4,
        Duration::from_secs(3),
    )
    .await?;
    let result = async {
        let store = Store::new(
            database.clone(),
            &format!("operator-http-{}", uuid::Uuid::new_v4()),
            Duration::from_secs(3),
        )?;
        schema::initialize_all(&store).await?;
        let authority = Authority::new(store, DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        authority
            .publish(
                0,
                &Configuration {
                    format: 2,
                    routing: json!({}),
                    policies: vec![],
                    model_policy: None,
                    credentials: vec![],
                    require_known_pricing: false,
                },
            )
            .await?;
        let router = router(
            authority.clone(),
            authenticator(vec![Grant::PolicyWrite])?,
            Duration::from_secs(2),
        );
        publication_cases(&authority, &router).await?;

        let pending =
            Body::from_stream(futures::stream::pending::<std::io::Result<bytes::Bytes>>());
        let unauthorized = Request::builder()
            .method(Method::POST)
            .uri("/operator/v1/mutations")
            .body(pending)?;
        let response = tokio::time::timeout(
            Duration::from_millis(100),
            router.clone().oneshot(unauthorized),
        )
        .await??;
        let error: ErrorResponse = decode(response, StatusCode::UNAUTHORIZED).await?;
        ensure!(error.code == ErrorCode::Unauthorized);

        let mutation = input("..");
        let receipt: Receipt = decode(
            router.clone().oneshot(post(&mutation)?).await?,
            StatusCode::OK,
        )
        .await?;
        ensure!(receipt.operation_id == ".." && receipt.operator_id == "writer");
        let lookup: OperationLookup = decode(
            router
                .clone()
                .oneshot(get("operations", &[("operation_id", "..")], WRITE_TOKEN)?)
                .await?,
            StatusCode::OK,
        )
        .await?;
        ensure!(matches!(lookup,OperationLookup::Committed {receipt:other} if *other==receipt));
        let lookup: OperationLookup = decode(
            router
                .clone()
                .oneshot(get("operations", &[("operation_id", "..")], READ_TOKEN)?)
                .await?,
            StatusCode::OK,
        )
        .await?;
        ensure!(matches!(lookup, OperationLookup::Unresolved { .. }));
        for (path, query, token) in [
            ("revision", vec![], WRITE_TOKEN),
            ("principals", vec![("id", "foreign")], READ_TOKEN),
        ] {
            let error: ErrorResponse = decode(
                router.clone().oneshot(get(path, &query, token)?).await?,
                StatusCode::FORBIDDEN,
            )
            .await?;
            ensure!(error.code == ErrorCode::Forbidden);
        }
        let revoked = super::router(
            authority.clone(),
            authenticator(vec![Grant::Read])?,
            Duration::from_secs(2),
        );
        let error: ErrorResponse = decode(
            revoked
                .oneshot(get("operations", &[("operation_id", "..")], WRITE_TOKEN)?)
                .await?,
            StatusCode::FORBIDDEN,
        )
        .await?;
        ensure!(error.code == ErrorCode::Forbidden);
        for (path, query) in [
            (
                "operations",
                vec![("operation_id", ".."), ("operation_id", "other")],
            ),
            ("revision", vec![("namespace", "foreign")]),
            ("resources", vec![("id", "pool"), ("actor", "writer")]),
        ] {
            let error: ErrorResponse = decode(
                router
                    .clone()
                    .oneshot(get(path, &query, READ_TOKEN)?)
                    .await?,
                StatusCode::BAD_REQUEST,
            )
            .await?;
            ensure!(error.code == ErrorCode::InvalidInput);
        }
        media_cases(&authority, &router).await?;

        // An exclusive authority fence proves apply cannot reach its writes.
        // Cancellation must still report Unknown instead of inventing rollback.
        let block = authority.store().begin().await?;
        authority.fence_exclusive(&block).await?;
        let blocker_pid: i32 = block.query_one(Statement::from_string(DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid")).await?.context("blocker PID missing")?.try_get("", "pid")?;
        let short = super::router(
            authority.clone(),
            authenticator(vec![Grant::PolicyWrite])?,
            Duration::from_millis(200),
        );
        let response = tokio::time::timeout(Duration::from_millis(600), async {
            let request = short.clone().oneshot(post(&input("blocked"))?);
            tokio::pin!(request);
            let observed = async {
                loop {
                    // Observe this exact holder, not an unrelated busy database.
                    // https://www.postgresql.org/docs/current/functions-info.html#FUNCTIONS-INFO-SESSION-TABLE
                    let row = database.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))) AS waiting",
                        [blocker_pid.into()])).await?.context("lock observation missing")?;
                    if row.try_get::<bool>("", "waiting")? { return Ok::<(), anyhow::Error>(()); }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            };
            tokio::select! {
                response = &mut request => { response?; anyhow::bail!("request ended before its SQL wait was observed"); },
                result = observed => result?,
            }
            Ok::<_, anyhow::Error>(request.await?)
        }).await.context("operator ignored whole-request deadline")??;
        let error: ErrorResponse = decode(response, StatusCode::SERVICE_UNAVAILABLE).await?;
        ensure!(error.code == ErrorCode::Unknown && error.operation_id.as_deref() == Some("blocked"));
        let response = tokio::time::timeout(Duration::from_millis(600), short.clone().oneshot(
            get("resources", &[("id", "blocked")], READ_TOKEN)?
        )).await.context("operator read ignored whole-request deadline")??;
        let error: ErrorResponse = decode(response, StatusCode::SERVICE_UNAVAILABLE).await?;
        ensure!(error.code == ErrorCode::Unavailable && error.operation_id.is_none());
        let released = block.rollback().await;
        released?;
        let pending = Body::from_stream(futures::stream::pending::<std::io::Result<bytes::Bytes>>());
        let mut request = post(&input("unread"))?;
        *request.body_mut() = pending;
        let response = tokio::time::timeout(Duration::from_millis(600), short.oneshot(request))
            .await.context("operator body read ignored whole-request deadline")??;
        let error: ErrorResponse = decode(response, StatusCode::SERVICE_UNAVAILABLE).await?;
        ensure!(error.code == ErrorCode::Unknown && error.operation_id.is_none());
        let lookup: OperationLookup = decode(
            router
                .oneshot(get(
                    "operations",
                    &[("operation_id", "blocked")],
                    WRITE_TOKEN,
                )?)
                .await?,
            StatusCode::OK,
        )
        .await?;
        ensure!(matches!(lookup, OperationLookup::Unresolved { .. }));
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

async fn record_snapshot(store: &Store) -> Result<String> {
    store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY id),'[]'::jsonb)::text AS snapshot FROM gateway_shared_records r WHERE namespace=$1",
        [store.namespace().into()]
    )).await?.context("missing authority record snapshot")?.try_get("", "snapshot").map_err(Into::into)
}

async fn publication_cases(authority: &Authority, policy_writer: &Router) -> Result<()> {
    let pool = Mutation {
        operation_id: "publication-pool".into(),
        command: Command::PutResource(ResourcePut {
            resource_pool_id: "publication-pool".into(),
            expected_revision: None,
            concurrent_attempts: 3,
        }),
    };
    let _: Receipt = decode(
        policy_writer.clone().oneshot(post(&pool)?).await?,
        StatusCode::OK,
    )
    .await?;
    let credentials = super::router(
        authority.clone(),
        authenticator(vec![Grant::CredentialWrite])?,
        Duration::from_secs(2),
    );
    let import = Mutation {
        operation_id: "publication-credential".into(),
        command: Command::ImportCredential(CredentialImport {
            provider: "fixture".into(),
            account: "default".into(),
            resource_pool_id: "publication-pool".into(),
            expected_version: None,
            credential: CredentialInput::ApiKey {
                value: "publication-private-secret".into(),
            },
        }),
    };
    let _: Receipt = decode(credentials.oneshot(post(&import)?).await?, StatusCode::OK).await?;
    let document: bitrouter_ai::policy::Document = serde_json::from_value(json!({
        "spec_version":"policy/1.0.0-beta.1",
        "policies": {
            "auto:cost": {"tiers":{"strong":{"model":"fixture:strong"},"economy":{"model":"fixture:economy"}},
                "default_tier":"strong","tool_use_tier":"strong","tool_safe_tiers":["strong"]},
            "review": {"tiers":{"safe":{"model":"fixture:economy"}},"default_tier":"safe"}
        }
    }))?;
    let configuration = Publication {
        format: 2,
        routing: json!({
            "providers":{"fixture":{"api_base":"https://example.com/v1","models":[{"id":"strong"},{"id":"economy"}]}},
            "routers":{
                "coding":{"selection":{"kind":"policy","policy":"auto:cost","base_model":"fixture:strong"}},
                "review":{"selection":{"kind":"policy","policy":"review","base_model":"fixture:economy"}}
            }
        }),
        policies: vec![
            json!({"id":"publication-acl","allowed_models":["fixture:strong","fixture:economy"]}),
        ],
        model_policy: Some(document.clone()),
        credentials: vec![CredentialBinding {
            provider: "fixture".into(),
            account: "default".into(),
            credential_provider: "fixture".into(),
            credential_account: "default".into(),
        }],
        require_known_pricing: false,
    };
    let mutation = Mutation {
        operation_id: "publish-current-source".into(),
        command: Command::Publish(PublicationPut {
            expected_generation: authority.head().await?.value.config_generation,
            configuration: configuration.clone(),
        }),
    };
    // The existing policy writer retains its narrower permissions.
    let before = record_snapshot(authority.store()).await?;
    let error: ErrorResponse = decode(
        policy_writer.clone().oneshot(post(&mutation)?).await?,
        StatusCode::FORBIDDEN,
    )
    .await?;
    ensure!(error.code == ErrorCode::Forbidden);
    ensure!(record_snapshot(authority.store()).await? == before);
    let publisher = super::router(
        authority.clone(),
        authenticator(vec![Grant::Publish])?,
        Duration::from_secs(2),
    );
    let receipt: Receipt = decode(
        publisher.clone().oneshot(post(&mutation)?).await?,
        StatusCode::OK,
    )
    .await?;
    let head = authority.head().await?;
    let loaded = authority.configuration(&head).await?;
    ensure!(
        serde_json::to_value(&loaded)? == serde_json::to_value(&configuration)?,
        "HTTP publication lost complete configuration source"
    );
    ensure!(serde_json::to_value(&loaded.model_policy)? == serde_json::to_value(Some(&document))?);
    let revision: Revision = decode(
        publisher
            .clone()
            .oneshot(get("revision", &[], READ_TOKEN)?)
            .await?,
        StatusCode::OK,
    )
    .await?;
    ensure!(
        revision.config_generation == head.value.config_generation
            && revision.key_generation == head.value.key_generation
    );
    ensure!(matches!(&receipt.outcome, Outcome::Published(actual) if *actual == revision));
    let credential: CredentialMetadata = decode(
        publisher
            .clone()
            .oneshot(get(
                "credentials",
                &[("provider", "fixture"), ("account", "default")],
                READ_TOKEN,
            )?)
            .await?,
        StatusCode::OK,
    )
    .await?;
    ensure!(credential.version == 1 && credential.resource_pool_id == "publication-pool");
    let before = record_snapshot(authority.store()).await?;
    let replay: Receipt = decode(
        publisher.clone().oneshot(post(&mutation)?).await?,
        StatusCode::OK,
    )
    .await?;
    ensure!(replay == receipt && record_snapshot(authority.store()).await? == before);
    let old_source = json!({"lockfileVersion":4,"policies":{"coding":{}}});
    for case in [
        "old-format",
        "old-field",
        "mixed-fields",
        "old-source",
        "missing-format",
        "future-format",
    ] {
        let operation_id = format!("publish-http-{case}");
        let mut raw = serde_json::to_value(&mutation)?;
        raw["operation_id"] = operation_id.clone().into();
        raw["command"]["input"]["expected_generation"] = revision.config_generation.into();
        let config = raw["command"]["input"]["configuration"]
            .as_object_mut()
            .context("publication object missing")?;
        match case {
            "old-format" => {
                config.insert("format".into(), 1.into());
            }
            "old-field" => {
                config.remove("model_policy");
                config.insert("policy_lock".into(), old_source.clone());
            }
            "mixed-fields" => {
                config.insert("policy_lock".into(), old_source.clone());
            }
            "old-source" => {
                config.insert("model_policy".into(), old_source.clone());
            }
            "missing-format" => {
                config.remove("format");
            }
            _ => {
                config.insert("format".into(), 3.into());
            }
        }
        let mut request = post(&mutation)?;
        *request.body_mut() = Body::from(serde_json::to_vec(&raw)?);
        let error: ErrorResponse = decode(
            publisher.clone().oneshot(request).await?,
            StatusCode::BAD_REQUEST,
        )
        .await?;
        ensure!(
            error.code == ErrorCode::InvalidInput && error.operation_id.is_none(),
            "raw publication accepted: {case}"
        );
        let lookup: OperationLookup = decode(
            publisher
                .clone()
                .oneshot(get(
                    "operations",
                    &[("operation_id", operation_id.as_str())],
                    WRITE_TOKEN,
                )?)
                .await?,
            StatusCode::OK,
        )
        .await?;
        ensure!(matches!(lookup, OperationLookup::Unresolved { .. }));
        let current = authority.head().await?;
        ensure!(
            serde_json::to_value(&current.value)? == serde_json::to_value(&head.value)?,
            "rejection changed publication head: {case}"
        );
        ensure!(
            serde_json::to_value(authority.configuration(&current).await?)?
                == serde_json::to_value(&loaded)?,
            "rejection changed configuration/bindings: {case}"
        );
        let current_credential: CredentialMetadata = decode(
            publisher
                .clone()
                .oneshot(get(
                    "credentials",
                    &[("provider", "fixture"), ("account", "default")],
                    READ_TOKEN,
                )?)
                .await?,
            StatusCode::OK,
        )
        .await?;
        ensure!(current_credential == credential);
        ensure!(
            record_snapshot(authority.store()).await? == before,
            "rejection changed authority records: {case}"
        );
    }
    Ok(())
}

async fn media_cases(authority: &Authority, router: &Router) -> Result<()> {
    let writer = Actor::new("writer".into(), [Grant::PolicyWrite].into())?;
    let reader = Actor::new("reader".into(), [Grant::Read].into())?;
    let operator = Operator::new(authority.clone());
    for (index, media) in [
        "application/json; a=",
        "application/json; a=\"escaped-end\\\"",
        "application/json; a=\"ok\"trailing",
        "application/json, text/plain",
        "duplicate",
        "encoding",
    ]
    .into_iter()
    .enumerate()
    {
        let mutation = input(&format!("invalid-media-{index}"));
        let mut request = post(&mutation)?;
        match media {
            "duplicate" => {
                request
                    .headers_mut()
                    .append(header::CONTENT_TYPE, "application/json".parse()?);
            }
            "encoding" => {
                request
                    .headers_mut()
                    .insert(header::CONTENT_ENCODING, "gzip".parse()?);
            }
            _ => {
                request
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, media.parse()?);
            }
        }
        let error: ErrorResponse = decode(
            router.clone().oneshot(request).await?,
            StatusCode::BAD_REQUEST,
        )
        .await?;
        ensure!(error.code == ErrorCode::InvalidInput);
        ensure!(matches!(
            operator.lookup(&writer, &mutation.operation_id).await?,
            OperationLookup::Unresolved { .. }
        ));
        let Command::PutResource(resource) = &mutation.command else {
            anyhow::bail!("fixture resource missing");
        };
        let absent = operator
            .resource(&reader, &resource.resource_pool_id)
            .await
            .err()
            .context("invalid media mutated resource")?;
        ensure!(matches!(
            absent.downcast_ref::<Failure>(),
            Some(Failure(ErrorCode::NotFound))
        ));
    }
    for (index, media) in [
        "application/json; empty=\"\"",
        r#"application/json; note="a,b;c\"d\\e""#,
        "application/json;; ;",
    ]
    .into_iter()
    .enumerate()
    {
        let mutation = input(&format!("valid-media-{index}"));
        let mut request = post(&mutation)?;
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, media.parse()?);
        let receipt: Receipt =
            decode(router.clone().oneshot(request).await?, StatusCode::OK).await?;
        ensure!(receipt.operation_id == mutation.operation_id);
    }
    Ok(())
}
