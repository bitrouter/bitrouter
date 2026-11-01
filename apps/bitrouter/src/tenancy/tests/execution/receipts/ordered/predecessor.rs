//! An actual v1 process must reject v2 before executing its unrestricted route.
use super::*;
use bitrouter_ai::{
    artifact::ArtifactIdentity,
    tenancy::{
        authority::{
            BindingPut, CredentialCreate, GrantScope, Permission, PrincipalPut, ResourceKey,
            TenantPut, TenantRole, TenantState,
        },
        configuration::{ConfigurationPut, ResourceRef},
        identity::{BindingId, PrincipalId, TenantId, Timestamp},
        operation::{DispatchState, ErrorCode, OperationStatus, TenantCapabilities},
        resources::{CredentialBinding, Definition, ResourcePut},
    },
};
use bitrouter_sdk::{error::Error as SdkError, gateway::tenancy::Error as DispatchError};
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio};
use tokio::process::{Child, Command};

const BASELINE: &str = "07cc1bb4812b27a40582b22298fff125dca50ec5";
const DEADLINE: Duration = Duration::from_secs(30);

fn command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("BITROUTER_HOME", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root)
        .env("ORDERED_TEST_OPERATOR", OPERATOR)
        // Synthetic 32-byte wrapping key; never inherited from the invoking host.
        .env(
            "ORDERED_TEST_WRAP",
            "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=",
        )
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

struct Predecessor {
    child: Child,
    endpoint: String,
    control: String,
    http2: bool,
    _root: tempfile::TempDir,
}

impl Predecessor {
    async fn start() -> Result<Self> {
        let binary = std::path::PathBuf::from(
            std::env::var_os("BITROUTER_PREDECESSOR_BRO")
                .context("set BITROUTER_PREDECESSOR_BRO to the actual 07cc1bb executable")?,
        );
        ensure!(
            binary.is_absolute() && binary.is_file(),
            "absolute predecessor binary required"
        );
        let root = tempfile::Builder::new()
            .prefix("ordered-predecessor-")
            .tempdir()?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        let identity = tokio::time::timeout(
            DEADLINE,
            command(&binary, root.path())
                .args(["--json", "build-info"])
                .stdout(Stdio::piped())
                .output(),
        )
        .await
        .context("predecessor identity deadline")??;
        ensure!(
            identity.status.success(),
            "predecessor identity command failed"
        );
        let artifact: ArtifactIdentity = serde_json::from_slice(&identity.stdout)?;
        ensure!(
            artifact.source_revision.as_deref() == Some(BASELINE),
            "wrong predecessor source revision"
        );

        let inference = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let control = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let inference_addr = inference.local_addr()?;
        let control_addr = control.local_addr()?;
        let config = root.path().join("bitrouter.yaml");
        // Only the predecessor initializes this fresh SQLite file. In particular,
        // the current process never runs its newer store migrations here.
        // Its unknown control endpoints require control:read before returning
        // 404, so discovery cannot fail merely because the operator lacks scope.
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "inherit_defaults":false, "registry":{"enabled":false},
                "server":{"listen":inference_addr.to_string(),"skip_auth":false,
                    "control_socket":root.path().join("control.sock")},
                "database":{"url":"sqlite://authority.sqlite?mode=rwc"},
                "control":{"enabled":true,"listen":control_addr.to_string(),"credentials":[
                    {"id":"operator","token_env":"ORDERED_TEST_OPERATOR","scopes":["control:tenancy","control:read"]}
                ]},
                "tenancy":{"enabled":true,"wrapping_key_env":"ORDERED_TEST_WRAP"}
            }))?,
        )?;
        drop((inference, control));
        let child = command(&binary, root.path())
            .args(["serve", "--config"])
            .arg(config)
            .spawn()?;
        Ok(Self {
            child,
            endpoint: format!("http://{inference_addr}"),
            control: format!("http://{control_addr}"),
            http2: artifact.features.iter().any(|feature| feature == "http2"),
            _root: root,
        })
    }

    fn control(&self, version: HttpVersion) -> Result<TenancyClient> {
        Ok(TenancyClient::with_options(
            &self.control,
            OPERATOR,
            TenancyOptions {
                http_version: version,
                request_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )?)
    }

    async fn ready(&mut self) -> Result<TenantCapabilities> {
        let control = self.control(HttpVersion::Http1Only)?;
        tokio::time::timeout(DEADLINE, async {
            loop {
                ensure!(
                    self.child.try_wait()?.is_none(),
                    "predecessor exited before readiness"
                );
                if let Ok(caps) = control.capabilities().await
                    && caps.dispatch_enabled
                    && caps.strict_receipts
                    && caps.revision_bound_dispatch
                {
                    return Ok(caps);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("predecessor readiness deadline")?
    }

    async fn stop(&mut self) -> Result<()> {
        self.child.start_kill()?;
        tokio::time::timeout(DEADLINE, self.child.wait())
            .await
            .context("predecessor reap deadline")??;
        Ok(())
    }
}

struct Provisioned {
    tenant: TenantId,
    route: ResourceRef,
    revision: ConfigurationRevision,
    credential: CredentialIssued,
}

async fn provision(
    control: &TenancyClient,
    caps: &TenantCapabilities,
    base: &str,
) -> Result<Provisioned> {
    let tenant: TenantId = "predecessor-tenant".try_into()?;
    let principal: PrincipalId = "predecessor-principal".try_into()?;
    let binding: BindingId = "predecessor-binding".try_into()?;
    let installation = &caps.installation_id;
    let scope = GrantScope::Tenant {
        operator_pools: Vec::new(),
    };
    let expiry: Timestamp = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .try_into()?;
    let result = control
        .put_tenant(
            &tenant,
            &TenantPut {
                operation_id: "predecessor-tenant".try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: installation.clone(),
            },
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    let result = control
        .put_principal(
            &principal,
            &PrincipalPut {
                operation_id: "predecessor-principal".try_into()?,
                expected_revision: None,
                issuer: "compatibility-test".into(),
                subject: "scoped-caller".into(),
                installation_id: installation.clone(),
            },
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    let result = control
        .put_binding(
            &tenant,
            &binding,
            &BindingPut {
                operation_id: "predecessor-binding".try_into()?,
                expected_revision: None,
                principal_id: principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: scope.clone(),
                expires_at: expiry.clone(),
                expected_management_owner: None,
                installation_id: installation.clone(),
            },
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    let key = |kind, id: &str| -> Result<ResourceKey> {
        Ok(ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
        })
    };
    let slot = key(ResourceKind::Credential, "predecessor-slot")?;
    let result = control
        .put_resource(
            &tenant,
            &ResourcePut {
                format: 1,
                operation_id: "predecessor-slot".try_into()?,
                installation_id: installation.clone(),
                key: slot.clone(),
                expected_revision: None,
                definition: Definition::Credential {
                    secret: "owned-upstream-secret".into(),
                },
            },
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    let slot = ResourceRef {
        owner_kind: slot.owner_kind,
        owner_id: slot.owner_id,
        resource_kind: slot.resource_kind,
        resource_id: slot.resource_id,
        revision_id: result.resource_revision.context("slot revision")?,
    };
    let route = key(ResourceKind::Route, MODEL)?;
    let result = control.put_resource(&tenant, &ResourcePut {
        format: 1, operation_id: "predecessor-route".try_into()?,
        installation_id: installation.clone(), key: route.clone(), expected_revision: None,
        definition: Definition::Route {
            // The full v1 route sends the failing candidate before the success.
            // A v2 subset selecting ordinal 1 must never fall back to that route.
            routing: json!({
                "providers":{
                    "first":{"api_base":base,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-1"}]},
                    "last":{"api_base":base,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-0"}]}
                },
                "models":{"predecessor-root":{"strategy":"priority","endpoints":[
                    {"provider":"first","service_id":"model-1"},
                    {"provider":"last","service_id":"model-0"}
                ]}}
            }),
            selector: "predecessor-root".into(), policy: None,
            credentials: vec![
                CredentialBinding {
                    provider: "first".into(), account: "default".into(), credential: slot.clone(),
                },
                CredentialBinding {
                    provider: "last".into(), account: "default".into(), credential: slot,
                },
            ],
        },
    }).await?;
    ensure!(result.status == OperationStatus::Applied);
    let route = ResourceRef {
        owner_kind: route.owner_kind,
        owner_id: route.owner_id,
        resource_kind: route.resource_kind,
        resource_id: route.resource_id,
        revision_id: result.resource_revision.context("route revision")?,
    };
    let credential = control
        .issue_credential(
            &tenant,
            &CredentialCreate {
                operation_id: "predecessor-key".try_into()?,
                expected_authority_revision: result
                    .authority_revision
                    .context("route authority")?,
                principal_id: principal,
                permissions: vec![Permission::ExecutionSubmit, Permission::CredentialUse],
                scope,
                expires_at: expiry,
                rotate_credential_id: None,
                binding_id: binding,
                installation_id: installation.clone(),
            },
        )
        .await?;
    let input: ConfigurationPut = serde_json::from_value(json!({
        "operation_id":"predecessor-config","installation_id":installation,
        "expected_authority_revision":credential.authority_revision,
        "document":{"routes":[route],"policy":null,
            "generation_defaults":{"max_output_tokens":25,"temperature":0.0},
            "learning":{"mode":"off","max_evaluations":0},
            "limits":{"active_executions":4,"queued_work":4,"learning_jobs":0}}
    }))?;
    let revision = control.put_configuration(&tenant, &input).await?;
    Ok(Provisioned {
        tenant,
        route,
        revision,
        credential,
    })
}

fn observed(provider: &Provider) -> Result<Vec<String>> {
    let observed = provider
        .observed
        .lock()
        .map_err(|_| anyhow::anyhow!("provider history poisoned"))?;
    ensure!(
        observed
            .iter()
            .all(|(_, headers)| !headers.contains_key("bitrouter-candidate-selection")),
        "dispatch metadata reached provider"
    );
    Ok(observed.iter().map(|(model, _)| model.clone()).collect())
}

async fn v1_control(
    gateway: &GatewayClient,
    caps: &TenantCapabilities,
    provisioned: &Provisioned,
    correlation: &str,
    request: &ChatRequest,
) -> Result<ExecutionId> {
    let binding = caps.bind(
        provisioned.revision.revision_id.clone(),
        correlation.try_into()?,
    )?;
    let reply = gateway
        .tenant(
            provisioned
                .credential
                .secret
                .as_deref()
                .context("scoped credential")?,
            &binding,
        )?
        .chat_reply(request)
        .await?;
    ensure!(reply.metadata.status == 200);
    let Body::Json(body) = reply.body else {
        anyhow::bail!("v1 JSON control returned a stream")
    };
    ensure!(serde_json::to_string(&body)?.contains("receipt reply"));
    reply.metadata.execution_id.context("v1 execution identity")
}

async fn qualify(
    server: &mut Predecessor,
    provider: &Provider,
    base: &str,
    version: HttpVersion,
) -> Result<()> {
    let caps = server.ready().await?;
    let control = server.control(HttpVersion::Http1Only)?;
    let provisioned = provision(&control, &caps, base).await?;
    let mut expected = Vec::new();
    let control = server.control(version)?;
    let caps = control.capabilities().await?;
    let discovery = control
        .ordered_capabilities()
        .await
        .err()
        .context("predecessor advertised v2")?;
    ensure!(
        matches!(discovery, SdkError::Tenancy(ref failure) if failure.status == 404),
        "ordered discovery did not report unsupported endpoint"
    );
    ensure!(observed(provider)? == expected);
    let gateway = GatewayClient::with_options(
        &server.endpoint,
        None,
        TransportOptions {
            http_version: version,
            request_timeout: Some(DEADLINE),
            ..Default::default()
        },
    )?;
    let request: ChatRequest = serde_json::from_value(json!({
        "model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":false
    }))?;
    let before = v1_control(&gateway, &caps, &provisioned, "before-rejection", &request).await?;
    expected.extend(["model-1".to_owned(), "model-0".to_owned()]);
    ensure!(
        observed(provider)? == expected,
        "v1 control did not execute the full fallback route"
    );
    // Deliberately bind the predecessor's real identities without pretending
    // its v1 discovery negotiated v2. This is the unsupported-version probe.
    let binding = OrderedDispatchBindingV2 {
        contract_version: "2".into(),
        installation_id: caps.installation_id.clone(),
        instance_id: caps.instance_id.clone(),
        capability_revision: caps.capability_revision.clone(),
        execution_revision: provisioned.revision.revision_id.clone(),
        correlation_id: "unsupported-v2".try_into()?,
        receipt_mode: "strict".into(),
        selection: OrderedCandidateSelectionV1 {
            schema_version: 1,
            route: provisioned.route.clone(),
            candidate_indices: vec![1],
        },
    };
    let error = gateway
        .ordered_tenant(
            provisioned
                .credential
                .secret
                .as_deref()
                .context("scoped credential")?,
            &binding,
        )?
        .chat_reply(&request)
        .await
        .err()
        .context("predecessor executed v2")?;
    ensure!(error.dispatch_state() == DispatchState::NotStarted);
    let DispatchError::Response(failure) = error else {
        anyhow::bail!("v2 never reached predecessor HTTP")
    };
    ensure!(
        failure.metadata.status == 400
            && failure.problem.is_none()
            && failure.metadata.execution_id.is_none()
            && failure.metadata.error_source.is_none()
    );
    let error = failure
        .tenancy_error
        .context("predecessor typed rejection")?;
    ensure!(
        error.code == ErrorCode::InvalidRequest
            && error.dispatch_state == DispatchState::NotStarted
            && error.execution_id.is_none()
    );
    ensure!(
        observed(provider)? == expected,
        "rejected v2 sent an upstream request"
    );
    let page = control
        .executions(
            &caps.installation_id,
            &provisioned.tenant,
            &binding.correlation_id,
            None,
            10,
        )
        .await?;
    ensure!(
        page.execution_ids.is_empty(),
        "rejected v2 created an execution"
    );
    ensure!(
        server.child.try_wait()?.is_none(),
        "predecessor exited after rejecting v2"
    );
    let after = v1_control(&gateway, &caps, &provisioned, "after-rejection", &request).await?;
    ensure!(before != after);
    expected.extend(["model-1".to_owned(), "model-0".to_owned()]);
    ensure!(
        observed(provider)? == expected,
        "post-rejection v1 control changed provider sends"
    );
    Ok(())
}

async fn qualify_transport(version: HttpVersion) -> Result<bool> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let provider = Provider {
        observed: Arc::new(Mutex::new(Vec::new())),
        held_provider: Arc::new(Mutex::new(None)),
        malformed: None,
    };
    let router = Router::new()
        .route("/v1/chat/completions", post(ordered_provider))
        .with_state(provider.clone());
    let _provider =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let mut server = Predecessor::start().await?;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        qualify(&mut server, &provider, &base, version),
    )
    .await
    .context("predecessor compatibility deadline");
    // Always kill and reap before removing the private directory, also when a
    // readiness, provisioning or assertion deadline fails. Logs are not printed.
    let stopped = server.stop().await;
    result??;
    stopped?;
    Ok(server.http2)
}

#[tokio::test]
#[ignore = "requires BITROUTER_PREDECESSOR_BRO pointing to the actual 07cc1bb executable; enable http2 when the predecessor supports it"]
async fn actual_predecessor_rejects_ordered_v2_without_executing_v1_route() -> Result<()> {
    // Each transport gets a fresh process and authority store so the two
    // deliberate 503 controls cannot open the other transport's circuit.
    let http2 = qualify_transport(HttpVersion::Http1Only).await?;
    #[cfg(feature = "http2")]
    if http2 {
        ensure!(qualify_transport(HttpVersion::Http2PriorKnowledge).await?);
    }
    #[cfg(not(feature = "http2"))]
    ensure!(
        !http2,
        "build this test with http2 to qualify this predecessor's transports"
    );
    Ok(())
}
