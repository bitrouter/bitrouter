use super::*;
use bitrouter_ai::tenancy::{
    authority::{
        BindingPut, CredentialCreate, GrantScope, Permission, PrincipalPut, TenantPut, TenantRole,
        TenantState,
    },
    configuration::{Activation, ConfigurationPut},
    identity::{InstallationId, PrincipalId, TenantId},
};
use bitrouter_sdk::tenancy::TenancyClient;
use std::os::unix::fs::PermissionsExt;
use tokio_util::task::AbortOnDropHandle;

pub(super) const OPERATOR: &str = "recovery-cli-operator-0123456789abcdef";
pub(super) const KEY_ENV: &str = "BITROUTER_TEST_RECOVERY_KEY";
pub(super) const OPERATOR_ID: &str = "recovery-operator";

pub(super) struct Live {
    service: crate::tenancy::Service,
    db: sea_orm::DatabaseConnection,
    pub base: String,
    stop: CancellationToken,
    server: AbortOnDropHandle<std::io::Result<()>>,
}
impl Live {
    pub async fn open(path: &Path, manifest: Option<&Path>, key_env: Option<&str>) -> Result<Self> {
        let database =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}", path.display())).await?;
        crate::db::run_migrations(database.database()).await?;
        let db = database.database().clone();
        let settings = crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: key_env.map(str::to_owned),
            resource_import: manifest.map(Path::to_path_buf),
            ..Default::default()
        };
        let service = crate::tenancy::Service::open_owned_profile(
            database,
            &settings,
            &crate::config::ContinuationConfig::default(),
        )
        .await?;
        let operators = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: OPERATOR_ID.into(),
                    token_env: "RECOVERY_OPERATOR_TOKEN".into(),
                    scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(OPERATOR.into()),
        )?;
        let router = crate::tenancy::http::router(service.clone(), operators);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let stop = CancellationToken::new();
        let signal = stop.clone();
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
        }));
        Ok(Self {
            service,
            db,
            base,
            stop,
            server,
        })
    }
    pub fn operator(&self) -> Result<TenancyClient> {
        Ok(TenancyClient::new(&self.base, OPERATOR)?)
    }
    pub async fn close(self, path: &Path) -> Result<()> {
        self.stop.cancel();
        let serving = self.server.await;
        let stopping = self.service.shutdown().await;
        drop(self.service);
        let closed = self.db.close().await;
        serving??;
        stopping?;
        closed?;
        let owner = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(owner) = crate::tenancy::ownership::acquire_path(path, false) {
                    break owner;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("real Service lease did not release")?;
        // Make this explicitly private stopped fixture eligible for offline
        // evidence. No process-wide umask or live SQLite descriptor is changed.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        drop(owner);
        Ok(())
    }
}
pub(super) struct Provision {
    pub installation: InstallationId,
    pub secret: String,
    pub operations: Vec<Value>,
    pub authority: bitrouter_ai::tenancy::identity::AuthorityRevision,
}
pub(super) async fn provision(live: &Live) -> Result<Provision> {
    let client = live.operator()?;
    let caps = client.capabilities().await?;
    ensure!(
        !caps.dispatch_enabled && !caps.strict_receipts && !caps.revision_bound_dispatch,
        "fixture enabled unsupported capabilities"
    );
    let installation = caps.installation_id;
    let tenant: TenantId = "tenant".try_into()?;
    let principal: PrincipalId = "owner".try_into()?;
    client
        .put_tenant(
            &tenant,
            &TenantPut {
                operation_id: "tenant".try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: installation.clone(),
            },
        )
        .await?;
    client
        .put_principal(
            &principal,
            &PrincipalPut {
                operation_id: "principal".try_into()?,
                expected_revision: None,
                issuer: "recovery-fixture".into(),
                subject: "owner".into(),
                installation_id: installation.clone(),
            },
        )
        .await?;
    let expires = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let binding = client
        .put_binding(
            &tenant,
            &"binding".try_into()?,
            &BindingPut {
                operation_id: "binding".try_into()?,
                expected_revision: None,
                principal_id: principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: GrantScope::Tenant {
                    operator_pools: vec![],
                },
                expires_at: expires.as_str().try_into()?,
                expected_management_owner: None,
                installation_id: installation.clone(),
            },
        )
        .await?;
    let issued = client
        .issue_credential(
            &tenant,
            &CredentialCreate {
                operation_id: "credential".try_into()?,
                expected_authority_revision: binding
                    .authority_revision
                    .context("binding revision")?,
                principal_id: principal,
                permissions: vec![
                    Permission::ResourceRead,
                    Permission::ResourceDraft,
                    Permission::PolicyActivate,
                ],
                scope: GrantScope::Tenant {
                    operator_pools: vec![],
                },
                expires_at: expires.as_str().try_into()?,
                rotate_credential_id: None,
                binding_id: "binding".try_into()?,
                installation_id: installation.clone(),
            },
        )
        .await?;
    let readback = client
        .credential(&installation, &tenant, &issued.credential_id)
        .await?;
    ensure!(
        readback.credential_id == issued.credential_id,
        "actual credential readback missing"
    );
    let mut operations = Vec::new();
    for id in ["tenant", "principal", "binding", "credential"] {
        operations.push(json!({"principal_id":PrincipalId::for_control_credential(OPERATOR_ID)?,"result":client.operation(&installation,&id.try_into()?).await?}));
    }
    Ok(Provision {
        installation,
        secret: issued.secret.context("one-time credential missing")?,
        operations,
        authority: issued.authority_revision,
    })
}
pub(super) fn manifest(installation: &InstallationId) -> Value {
    let reference = |kind: &str, id: &str| json!({"owner_kind":"tenant","owner_id":"tenant","resource_kind":kind,"resource_id":id,"revision_id":"v1"});
    json!({"format":1,"installation_id":installation,"resources":[
        {"reference":reference("credential","slot"),"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"PRIVATE_RECOVERY_PROVIDER_KEY"}}}},
        {"reference":reference("route","visible"),"definition":{"kind":"route","selector":"fixture:model-a","policy":null,"routing":{"providers":{"fixture":{"api_base":"http://127.0.0.1:1/v1","models":[{"id":"model-a"}]}}},"credentials":[{"provider":"fixture","account":"default","credential":reference("credential","slot")}]}}
    ]})
}
pub(super) async fn configure(
    live: &Live,
    provision: &mut Provision,
    source: &Provision,
) -> Result<Value> {
    let tenant: TenantId = "tenant".try_into()?;
    let client = TenancyClient::new(&live.base, &provision.secret)?;
    let manifest = manifest(&provision.installation);
    let route = manifest["resources"][1]["reference"].clone();
    let document = serde_json::from_value(
        json!({"routes":[route],"policy":null,"generation_defaults":{"max_output_tokens":128,"temperature":0.2},"learning":{"mode":"off","max_evaluations":0},"limits":{"active_executions":1,"queued_work":1,"learning_jobs":0}}),
    )?;
    let prepared = client
        .put_configuration(
            &tenant,
            &ConfigurationPut {
                operation_id: "draft".try_into()?,
                expected_authority_revision: provision.authority,
                document,
                installation_id: provision.installation.clone(),
            },
        )
        .await?;
    let draft = client
        .operation(&provision.installation, &"draft".try_into()?)
        .await?;
    let activation = client
        .activate(
            &tenant,
            &Activation {
                operation_id: "activate".try_into()?,
                expected_active_revision: None,
                revision_id: prepared.revision_id.clone(),
                expected_authority_revision: draft.authority_revision.context("draft authority")?,
                installation_id: provision.installation.clone(),
            },
        )
        .await?;
    let read = client
        .configuration(&provision.installation, &tenant, &prepared.revision_id)
        .await?;
    let active = client
        .active_revision(&provision.installation, &tenant)
        .await?;
    ensure!(
        serde_json::to_value(&read)? == serde_json::to_value(&prepared)?
            && active.revision_id.as_ref() == Some(&prepared.revision_id),
        "actual retained configuration/activation readback"
    );
    ensure!(
        TenancyClient::new(&live.base, &source.secret)?
            .configuration(&provision.installation, &tenant, &prepared.revision_id)
            .await
            .is_err(),
        "old source credential accepted by target"
    );
    ensure!(
        client
            .configuration(&source.installation, &tenant, &prepared.revision_id)
            .await
            .is_err(),
        "old installation header accepted by target"
    );
    provision
        .operations
        .push(json!({"principal_id":"owner","result":draft}));
    provision.operations.push(json!({"principal_id":"owner","result":client.operation(&provision.installation,&"activate".try_into()?).await?}));
    Ok(
        json!({"format":"bitrouter.offline.fresh-provisioning.v1","installation_id":provision.installation,"operations":provision.operations,"configurations":[prepared],"active":[{"tenant":tenant,"revision":active.revision_id,"authority_revision":activation.authority_revision}],"resources":manifest["resources"].as_array().context("manifest rows")?.iter().map(|r|r["reference"].clone()).collect::<Vec<_>>() }),
    )
}
pub(super) fn private_json(path: &Path, value: &Value) -> Result<Value> {
    let bytes = serde_json::to_vec_pretty(value)?;
    files::write_new(path, &bytes)?;
    Ok(json!({"path":path,"sha256":files::sha(&bytes)}))
}
