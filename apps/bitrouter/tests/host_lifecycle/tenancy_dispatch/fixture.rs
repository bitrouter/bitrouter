//! Real control-plane provisioning; no cfg(test) Service or direct SQL authority.
use super::*;
use base64::Engine;
use bitrouter_ai::tenancy::{
    authority::{
        BindingPut, CredentialCreate, GrantScope, PrincipalPut, TenantPut, TenantRole, TenantState,
    },
    configuration::{Activation, ConfigurationPut, ConfigurationRevision},
    identity::{BindingId, InstallationId, PrincipalId, TenantId, Timestamp},
};
use bitrouter_sdk::tenancy::TenancyOptions;

pub(super) struct Provisioned {
    pub operator: TenancyClient,
    pub reader: TenancyClient,
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub issued: CredentialIssued,
    pub revision: ConfigurationRevision,
    pub key: String,
}

pub(super) fn client(host: &Host, token: &str) -> Result<TenancyClient> {
    Ok(TenancyClient::with_options(
        &format!("http://{}", host.remote),
        token,
        TenancyOptions {
            connect_timeout: Duration::from_secs(1),
            request_timeout: STEP_TIMEOUT,
            max_response_bytes: BODY_LIMIT,
            ..TenancyOptions::default()
        },
    )?)
}

pub(super) async fn stop(host: &mut Host) -> Result<()> {
    ensure!(matches!(
        host.command(&DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    ensure!(host.exited().await?.success(), "owned host stop failed");
    host.ensure_closed().await
}

pub(super) async fn provision(host: &mut Host, upstream: &str) -> Result<Provisioned> {
    host.ready().await?;
    let operator = client(host, CONTROL_TOKEN)?;
    let initial = operator.capabilities().await?;
    eligible(&initial)?;
    let installation = &initial.installation_id;
    let tenant: TenantId = "dispatch-tenant".try_into()?;
    let principal: PrincipalId = "dispatch-principal".try_into()?;
    let binding: BindingId = "dispatch-binding".try_into()?;
    let created = operator
        .put_tenant(
            &tenant,
            &TenantPut {
                operation_id: "dispatch-tenant-create".try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: installation.clone(),
            },
        )
        .await?;
    applied(&created)?;
    let created = operator
        .put_principal(
            &principal,
            &PrincipalPut {
                operation_id: "dispatch-principal-create".try_into()?,
                expected_revision: None,
                issuer: "host-lifecycle".into(),
                subject: "actual-dispatch".into(),
                installation_id: installation.clone(),
            },
        )
        .await?;
    applied(&created)?;
    stop(host).await?;
    let reference = |kind: &str, id: &str| {
        json!({
            "owner_kind":"tenant", "owner_id":tenant, "resource_kind":kind,
            "resource_id":id, "revision_id":"v1",
        })
    };
    let slot = reference("credential", "dispatch-slot");
    let route = reference("route", MODEL);
    let manifest = json!({
        "format":1, "installation_id":installation,
        "resources":[
            {"reference":slot, "definition":{"kind":"credential", "credential":{"type":"api_key", "data":{"value":PROVIDER_KEY}}}},
            {"reference":route, "definition":{
                "kind":"route", "selector":"mock:model", "policy":null,
                "routing":{"providers":{"mock":{"api_base":upstream,"models":[{"id":"model"}]}}},
                "credentials":[{"provider":"mock", "account":"default", "credential":slot}],
            }},
        ],
    });
    let manifest_path = host.home.join("resources.json");
    fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600))?;
    let config = host.home.join("private.yaml");
    let original = fs::read_to_string(&config)?;
    ensure!(original.matches("tenancy: {enabled: true}").count() == 1);
    fs::write(&config, original.replace(
        "tenancy: {enabled: true}",
        "tenancy:\n  enabled: true\n  resource_import: resources.json\n  wrapping_key_env: TEST_TENANCY_WRAPPING_KEY",
    ))?;
    let key = base64::engine::general_purpose::STANDARD.encode([41; 32]);
    host.spawn_with_wrapping_key(&config, Some(&key))?;
    host.ready().await?;
    let imported = operator.capabilities().await?;
    eligible(&imported)?;
    ensure!(
        imported.installation_id == *installation && imported.instance_id != initial.instance_id
    );
    let expires_at: Timestamp = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .try_into()?;
    let scope = GrantScope::Tenant {
        operator_pools: Vec::new(),
    };
    let bound = operator
        .put_binding(
            &tenant,
            &binding,
            &BindingPut {
                operation_id: "dispatch-binding-create".try_into()?,
                expected_revision: None,
                principal_id: principal.clone(),
                role: TenantRole::Editor,
                scope: scope.clone(),
                expires_at: expires_at.clone(),
                expected_management_owner: None,
                installation_id: installation.clone(),
            },
        )
        .await?;
    applied(&bound)?;
    let issued = operator
        .issue_credential(
            &tenant,
            &CredentialCreate {
                operation_id: "dispatch-credential-create".try_into()?,
                expected_authority_revision: bound
                    .authority_revision
                    .context("binding authority")?,
                principal_id: principal.clone(),
                permissions: TenantRole::Editor.permissions().to_vec(),
                scope,
                expires_at,
                rotate_credential_id: None,
                binding_id: binding,
                installation_id: installation.clone(),
            },
        )
        .await?;
    ensure!(issued.tenant_id == tenant && issued.principal_id == principal);
    let status = operator
        .credential(installation, &tenant, &issued.credential_id)
        .await?;
    ensure!(
        status.state == CredentialState::Active
            && status.credential_revision == issued.credential_revision
    );
    let input: ConfigurationPut = serde_json::from_value(json!({
        "operation_id":"dispatch-draft", "installation_id":installation,
        "expected_authority_revision":issued.authority_revision,
        "document":{
            "routes":[route], "policy":null,
            "generation_defaults":{"max_output_tokens":64, "temperature":0.0},
            "learning":{"mode":"off", "max_evaluations":0},
            "limits":{"active_executions":1, "queued_work":0, "learning_jobs":0},
        },
    }))?;
    let revision = operator.put_configuration(&tenant, &input).await?;
    let draft = operator
        .operation(installation, &input.operation_id)
        .await?;
    applied(&draft)?;
    let active = operator
        .activate(
            &tenant,
            &Activation {
                operation_id: "dispatch-activate".try_into()?,
                expected_active_revision: None,
                revision_id: revision.revision_id.clone(),
                expected_authority_revision: draft.authority_revision.context("draft authority")?,
                installation_id: installation.clone(),
            },
        )
        .await?;
    applied(&active)?;
    ensure!(
        operator
            .active_revision(installation, &tenant)
            .await?
            .revision_id
            .as_ref()
            == Some(&revision.revision_id)
    );
    let reader = client(
        host,
        issued
            .secret
            .as_deref()
            .context("one-time tenant credential")?,
    )?;
    let readback = reader
        .configuration(installation, &tenant, &revision.revision_id)
        .await?;
    ensure!(serde_json::to_vec(&readback)? == serde_json::to_vec(&revision)?);
    ensure!(readback.document == input.document);
    Ok(Provisioned {
        operator,
        reader,
        installation: installation.clone(),
        tenant,
        principal,
        issued,
        revision,
        key,
    })
}
