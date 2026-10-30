//! Actual connector/Service/SDK provisioning, stopped import, then actual scoped HTTP reads.
use super::*;
use anyhow::Context;
use bitrouter_ai::{
    eval::{AdmissionStatus, EvalSnapshot, EvalSubject, EvaluationResult},
    tenancy::{
        authority::{
            BindingPut, CredentialCreate, GrantScope, Permission, PrincipalPut, TenantPut,
            TenantRole, TenantState,
        },
        identity::{PrincipalId, TenantId},
    },
};
use bitrouter_sdk::tenancy::TenancyClient;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;
mod cli;
mod negative;

const OPERATOR: &str = "offline-eval-test-operator-secret-32-bytes";
pub(super) const TIMEOUT: Duration = Duration::from_secs(110);
struct Live {
    service: crate::tenancy::Service,
    db: DatabaseConnection,
    base: String,
    stop: CancellationToken,
    server: AbortOnDropHandle<std::io::Result<()>>,
}
impl Live {
    async fn open(path: &Path, legacy: bool) -> Result<(Self, Vec<EvalSnapshot>)> {
        let database =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}", path.display())).await?;
        crate::db::run_migrations(database.database()).await?;
        let db = database.database().clone();
        let snapshots = if legacy {
            produce(&db).await?
        } else {
            Vec::new()
        };
        // The real connector's acquisition is consumed exactly once by Service.
        let service = crate::tenancy::Service::open_owned_profile(
            database,
            &crate::config::tenancy::TenancyConfig {
                enabled: true,
                ..Default::default()
            },
            &crate::config::ContinuationConfig::default(),
        )
        .await?;
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "offline-operator".into(),
                    token_env: "OFFLINE_EVAL_TEST_OPERATOR".into(),
                    scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(OPERATOR.into()),
        )?;
        let router = crate::tenancy::http::router(service.clone(), auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let stop = CancellationToken::new();
        let signal = stop.clone();
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
        }));
        Ok((
            Self {
                service,
                db,
                base,
                stop,
                server,
            },
            snapshots,
        ))
    }
    fn operator(&self) -> Result<TenancyClient> {
        Ok(TenancyClient::new(&self.base, OPERATOR)?)
    }
    async fn close(self, path: &Path) -> Result<()> {
        let Self {
            service,
            db,
            stop,
            server,
            ..
        } = self;
        stop.cancel();
        let server_result = server.await;
        let shutdown = service.shutdown().await;
        drop(service);
        let close = db.close().await;
        server_result??;
        shutdown?;
        close?;
        // Do not infer release from API close: the actual pool hook must drop.
        let owner = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(owner) = crate::tenancy::ownership::acquire_path(path, false) {
                    break owner;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        drop(owner);
        Ok(())
    }
}
fn subject(index: usize) -> Result<EvalSubject> {
    Ok(serde_json::from_value(json!({
        "schema_version":1,"eval_id":format!("legacy/subject {index} α?"),"scope":"request","subject_id":format!("request-{index}"),
        "policy_digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "holdout":false,"decisions":[],"requested_dimensions":[],"evidence":[],
        "evidence_digest":crate::storage::evaluation::evidence_digest(&[])?,"observed_at":"2026-08-01T00:00:00Z"
    }))?)
}
async fn produce(db: &DatabaseConnection) -> Result<Vec<EvalSnapshot>> {
    let store = crate::core::eval::store::EvalStore::new(db.clone());
    let mut snapshots = Vec::new();
    for (index, owner) in ["owner", "OWNER"].into_iter().enumerate() {
        let subject = subject(index)?;
        store.insert_subject_owned(&subject, owner).await?;
        let result: EvaluationResult = serde_json::from_value(json!({
            "schema_version":1,"eval_id":subject.eval_id,"evidence_digest":subject.evidence_digest,
            "evaluator":{"authority_id":"legacy-author","evaluator_id":"human","kind":"human","version":"1","config_digest":subject.policy_digest},
            "verdict":"pass","metrics":{},"hard_violations":[],"evidence_refs":[],"decision_credit":{},
            "idempotency_key":"same-owner-local-idempotency","submitted_at":"2026-08-01T00:01:00Z"
        }))?;
        let id = store
            .insert_result_owned(&result, owner)
            .await?
            .result_id()
            .to_owned();
        store
            .append_admission_event(
                &id,
                AdmissionStatus::Admitted,
                "original event",
                "legacy-author",
            )
            .await?;
        let snapshot = store
            .freeze_snapshot_for_owner("2026-08-01T08:02:00.000+08:00", owner)
            .await?;
        // A valid historical JSON spelling need not match the current serializer.
        // Retain this representation after confirming the ordinary Legacy reader.
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE eval_snapshots SET manifest_json=? WHERE evidence_root=?",
            [
                format!(" {}\n", serde_json::to_string_pretty(&snapshot)?).into(),
                snapshot.evidence_root.clone().into(),
            ],
        ))
        .await?;
        ensure!(
            store
                .snapshot_by_root_for_owner(&snapshot.evidence_root, owner)
                .await?
                == Some(snapshot.clone())
        );
        snapshots.push(snapshot);
    }
    // Wildcard evidence must remain in the explicit external quarantine partition.
    store.freeze_snapshot("2026-08-01T08:03:00+08:00").await?;
    Ok(snapshots)
}
struct Provision {
    installation: InstallationId,
    secrets: Vec<String>,
    operations: Vec<String>,
}
async fn provision(live: &Live, role: TenantRole) -> Result<Provision> {
    let client = live.operator()?;
    let caps = client.capabilities().await?;
    ensure!(
        !caps.dispatch_enabled && !caps.strict_receipts,
        "fixture enabled production dispatch"
    );
    let installation = caps.installation_id;
    let mut secrets = Vec::new();
    let mut operations = Vec::new();
    for index in 0..2 {
        let tenant: TenantId = format!("target-{index}").try_into()?;
        let principal: PrincipalId = format!("reader-{index}").try_into()?;
        let tenant_op = format!("tenant-{index}");
        let principal_op = format!("principal-{index}");
        let binding_op = format!("binding-{index}");
        let credential_op = format!("credential-{index}");
        client
            .put_tenant(
                &tenant,
                &TenantPut {
                    operation_id: tenant_op.as_str().try_into()?,
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
                    operation_id: principal_op.as_str().try_into()?,
                    expected_revision: None,
                    issuer: "offline-fixture".into(),
                    subject: principal.to_string(),
                    installation_id: installation.clone(),
                },
            )
            .await?;
        let binding = "reader-binding".try_into()?;
        let expiry = (chrono::Utc::now() + chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let applied = client
            .put_binding(
                &tenant,
                &binding,
                &BindingPut {
                    operation_id: binding_op.as_str().try_into()?,
                    expected_revision: None,
                    principal_id: principal.clone(),
                    role,
                    scope: GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    },
                    expires_at: expiry.as_str().try_into()?,
                    expected_management_owner: None,
                    installation_id: installation.clone(),
                },
            )
            .await?;
        let issued = client
            .issue_credential(
                &tenant,
                &CredentialCreate {
                    operation_id: credential_op.as_str().try_into()?,
                    expected_authority_revision: applied
                        .authority_revision
                        .context("binding authority revision")?,
                    principal_id: principal,
                    permissions: vec![Permission::ResourceRead],
                    scope: GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    },
                    expires_at: expiry.as_str().try_into()?,
                    rotate_credential_id: None,
                    binding_id: binding,
                    installation_id: installation.clone(),
                },
            )
            .await?;
        let actual = client
            .credential(&installation, &tenant, &issued.credential_id)
            .await?;
        ensure!(
            actual.credential_id == issued.credential_id,
            "actual credential readback"
        );
        secrets.push(issued.secret.context("one-time credential")?);
        operations.extend([tenant_op, principal_op, binding_op, credential_op]);
    }
    Ok(Provision {
        installation,
        secrets,
        operations,
    })
}
pub(super) struct Fixture {
    pub(super) directory: tempfile::TempDir,
    source: PathBuf,
    pub(super) target: PathBuf,
    bundle: PathBuf,
    snapshots: Vec<EvalSnapshot>,
    manifests: Vec<String>,
    provision: Provision,
    plan: Value,
}
impl Fixture {
    pub(super) async fn new() -> Result<Self> {
        Self::in_directory(tempfile::tempdir()?).await
    }
    pub(super) async fn in_directory(directory: tempfile::TempDir) -> Result<Self> {
        Self::in_directory_role(directory, TenantRole::TenantAdmin).await
    }
    async fn in_directory_role(directory: tempfile::TempDir, role: TenantRole) -> Result<Self> {
        Self::in_directory_role_with_unadmitted(directory, role, None).await
    }
    async fn in_directory_role_with_unadmitted(
        directory: tempfile::TempDir,
        role: TenantRole,
        unadmitted_owner: Option<&str>,
    ) -> Result<Self> {
        let source = directory.path().join("source.sqlite");
        let target = directory.path().join("target.sqlite");
        let (live, snapshots) = Live::open(&source, true).await?;
        let mut manifests = Vec::new();
        for owner in ["owner", "OWNER"] {
            let row = live
                .db
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT manifest_json FROM eval_snapshots WHERE owner_user_id=?",
                    [owner.into()],
                ))
                .await?
                .context("original manifest")?;
            manifests.push(row.try_get::<String>("", "manifest_json")?);
        }
        let observed = async {
            if let Some(owner) = unadmitted_owner {
                negative::produce_unadmitted(&live.db, owner).await?;
            }
            live.operator()?
                .capabilities()
                .await
                .map_err(anyhow::Error::from)
        }
        .await;
        let closed = live.close(&source).await;
        let observed = observed?;
        closed?;
        let (live, _) = Live::open(&target, false).await?;
        let provisioned = provision(&live, role).await;
        let closed = live.close(&target).await;
        let provision = provisioned?;
        closed?;
        let bundle = directory.path().join("source-bundle");
        let target_bundle = directory.path().join("target-bundle");
        let source_evidence = super::super::snapshot_inner(&source, &bundle).await?;
        let target_evidence = super::super::snapshot_inner(&target, &target_bundle).await?;
        ensure!(
            source_evidence
                .evidence
                .inventory
                .installation_id
                .as_deref()
                == Some(observed.installation_id.as_str())
        );
        let partitions = directory.path().join("partitions");
        inventory(
            &bundle,
            &source_evidence.evidence_digest,
            "local_user",
            &partitions,
        )
        .await?;
        let packet: Value = serde_json::from_slice(&files::read(
            &partitions.join("partitions.json"),
            2 * 1024 * 1024,
        )?)?;
        // Explicit offline operator decision, anchored independently from the M0 mapping.
        let ownership = json!({"source":observed.installation_id,"source_kind":"local_user","owners":["owner","OWNER"],"decision":"import named partitions; quarantine wildcard"});
        let ownership_bytes = serde_json::to_vec(&ownership)?;
        files::write_new(
            &directory.path().join("ownership-evidence.json"),
            &ownership_bytes,
        )?;
        let ownership_digest = files::sha(&ownership_bytes);
        let mut mappings = Vec::new();
        for part in packet["partitions"].as_array().context("partitions")? {
            let owner = part["owner"].as_str();
            let index = if owner == Some("owner") { 0 } else { 1 };
            mappings.push(json!({"source_installation_id":observed.installation_id,
                "source_kind":if owner.is_some(){"local_user"}else{"unresolved"},"old_owner":owner,
                "resource_kind":"evaluation","tenant_id":owner.map(|_|format!("target-{index}")),
                "principal_id":owner.map(|_|format!("reader-{index}")),"disposition":if owner.is_some(){"mapped"}else{"quarantined"},
                "evidence_digest":ownership_digest,"source_count":part["source_count"],"resource_examples":[],
                "rationale":"Explicit owner mapping","evidence_ref":"ownership-evidence.json","source_content_digest":part["source_content_digest"]}));
        }
        let mapping = json!({"schema_version":1,"source_installation_id":observed.installation_id,"source_commit":"1".repeat(40),
            "source_snapshot_digest":source_evidence.evidence.snapshot_digest,"mappings":mappings,"tenant_merge_approvals":[]});
        let mapping_bytes = serde_json::to_vec(&mapping)?;
        let manifest = directory.path().join("mapping.json");
        files::write_new(&manifest, &mapping_bytes)?;
        let plan = json!({"format":"bitrouter.offline.eval-backfill.v1","bundle":bundle,
            "database_evidence_digest":source_evidence.evidence_digest,"source_kind":"local_user","ownership_evidence_digest":ownership_digest,
            "ownership_evidence":directory.path().join("ownership-evidence.json"),"manifest":manifest,"manifest_digest":files::sha(&mapping_bytes),"target":target,"target_bundle":target_bundle,
            "target_evidence_digest":target_evidence.evidence_digest,"target_installation":provision.installation,
            "provisioning_operations":provision.operations});
        Ok(Self {
            directory,
            source,
            target,
            bundle,
            snapshots,
            manifests,
            provision,
            plan,
        })
    }
    pub(super) fn write_plan(&self, name: &str) -> Result<(PathBuf, String, PathBuf)> {
        let path = self.directory.path().join(format!("{name}.json"));
        let bytes = serde_json::to_vec(&self.plan)?;
        files::write_new(&path, &bytes)?;
        Ok((path, files::sha(&bytes), self.directory.path().join(name)))
    }
}

#[tokio::test]
async fn real_provision_stop_import_restart_scoped_reader_preserves_original_manifest() -> Result<()>
{
    tokio::time::timeout(TIMEOUT,async {
        let fixture=Fixture::new().await?;
        let source_before=std::fs::read(&fixture.source)?;
        let bundle_before=std::fs::read(fixture.bundle.join("snapshot.sqlite"))?;
        let (plan,anchor,report)=fixture.write_plan("apply")?;
        let result=apply::run(&plan,&anchor,&report).await?;
        let result=serde_json::to_value(result)?;
        ensure!(result["imported_counts"]==json!([2,2,2,2]) && result["provenance_rows"]==2
            && result["backfill_applied"]==true && result["complete"]==false && result["cutover_ready"]==false);
        ensure!(result["quarantined_eval"].as_array().context("quarantine")?.len()==1);
        ensure!(std::fs::read(&fixture.source)?==source_before && std::fs::read(fixture.bundle.join("snapshot.sqlite"))?==bundle_before);
        ensure!(report.join("RECORDED").exists() && !report.join("INCOMPLETE").exists());
        let (live,_)=Live::open(&fixture.target,false).await?;
        let checked=async {
            for index in 0..2 {
                let tenant:TenantId=format!("target-{index}").try_into()?;
                let client=TenancyClient::new(&live.base,&fixture.provision.secrets[index])?;
                let snapshot=client.eval_snapshot(&fixture.provision.installation,&tenant,&fixture.snapshots[index].evidence_root).await?;
                ensure!(snapshot==fixture.snapshots[index]);
                ensure!(client.eval_subject(&fixture.provision.installation,&tenant,&subject(index)?.eval_id).await?==subject(index)?);
                ensure!(client.eval_status(&fixture.provision.installation,&tenant).await?.results==1);
                ensure!(client.eval_snapshot(&fixture.provision.installation,&tenant,&fixture.snapshots[1-index].evidence_root).await.is_err(),"cross-tenant snapshot readable");
                let row=live.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "SELECT manifest_json FROM eval_snapshots WHERE installation_id=? AND tenant_id=?",[fixture.provision.installation.as_str().into(),tenant.as_str().into()])).await?.context("raw imported snapshot")?;
                ensure!(row.try_get::<String>("","manifest_json")?==fixture.manifests[index],"original serialization changed");
            }
            let legacy=crate::core::eval::store::EvalStore::new(live.db.clone());
            ensure!(legacy.list_subjects().await?.is_empty());
            ensure!(legacy.snapshot_by_root(&fixture.snapshots[0].evidence_root).await?.is_none());
            Ok::<_,anyhow::Error>(())
        }.await;
        let closed=live.close(&fixture.target).await;checked?;closed?;
        // A second import cannot replace/merge history, even if logically equal.
        let (plan,anchor,output)=fixture.write_plan("replay")?;
        ensure!(apply::run(&plan,&anchor,&output).await.is_err());
        Ok::<_,anyhow::Error>(())
    }).await?
}
