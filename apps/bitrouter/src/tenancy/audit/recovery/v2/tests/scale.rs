//! Complete streamed fresh-target proof beyond the old aggregate profiles.
use super::*;
use crate::offline::{
    engine::Connections,
    large::{self, Options, Source},
    recovery::{
        plan::Anchor,
        tests::fixture::scale::{
            FileWitness, JsonLines, append_principals, observation::Observation, witness,
        },
        v2::format::{ApprovalHeader, ProvisioningHeader, ProvisioningRecord},
    },
};
use bitrouter_ai::tenancy::{
    authority::{BindingPut, CredentialCreate, GrantScope, Permission, PrincipalPut, TenantPut},
    configuration::{Activation, ConfigurationPut, ResourceRef},
    identity::{InstallationId, OperationId, PrincipalId, TenantId},
    operation::{OperationResult, OperationStatus},
};
use bitrouter_sdk::tenancy::TenancyClient;
use sea_orm::sqlx::Row as _;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::OpenOptionsExt,
};

const TENANTS: u64 = 33;
const CREDENTIALS: u64 = 257;
const TAIL: u64 = 4097;
const OPERATIONS: u64 = 4519;
const RESOURCES: u64 = 297;
const FLOOR: u64 = 512 * 1024 * 1024;
const BUFFER: usize = 64 * 1024;
const RECORD: u64 = 1024 * 1024;

fn settings(home: &Path) -> Options {
    Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: FLOOR,
    }
}
fn bundle_witness(path: &Path) -> Result<[FileWitness; 4]> {
    Ok([
        witness(&path.join("READY"))?,
        witness(&path.join("evidence.json"))?,
        witness(&path.join("inventory.key"))?,
        witness(&path.join("snapshot.sqlite"))?,
    ])
}
fn tenant(index: u64) -> String {
    format!("scale-tenant-{index:02}")
}
fn principal(index: u64) -> String {
    format!("scale-owner-{index:02}")
}
fn reference(index: u64, route: Option<u64>) -> Result<ResourceRef> {
    Ok(serde_json::from_value(
        json!({"owner_kind":"tenant","owner_id":tenant(index),
        "resource_kind":if route.is_some(){"route"}else{"credential"},
        "resource_id":route.map_or_else(||"slot".to_owned(),|r|format!("route-{r:02}")),"revision_id":"v1"}),
    )?)
}
fn resource(index: u64, route: Option<u64>) -> Result<Value> {
    let resource_ref = reference(index, route)?;
    let definition = match route {
        None => {
            json!({"kind":"credential","credential":{"type":"api_key","data":{"value":"PRIVATE_SCALE_PROVIDER_KEY"}}})
        }
        Some(route) => {
            json!({"kind":"route","selector":format!("fixture:model-{route:02}"),"policy":null,
            "routing":{"providers":{"fixture":{"api_base":"http://127.0.0.1:1/v1","models":(0..8).map(|i|json!({"id":format!("model-{i:02}")})).collect::<Vec<_>>()}}},
            "credentials":[{"provider":"fixture","account":"default","credential":reference(index,None)?}]})
        }
    };
    Ok(json!({"reference":resource_ref,"definition":definition}))
}
fn document(index: u64) -> Result<bitrouter_ai::tenancy::configuration::Configuration> {
    let routes = (0..8)
        .map(|r| reference(index, Some(r)))
        .collect::<Result<Vec<_>>>()?;
    Ok(serde_json::from_value(
        json!({"routes":routes,"policy":null,
        "generation_defaults":{"max_output_tokens":128,"temperature":0.2},
        "learning":{"mode":"off","max_evaluations":0},"limits":{"active_executions":1,"queued_work":1,"learning_jobs":0}}),
    )?)
}
fn extra_resource() -> Result<Value> {
    let mut value = resource(32, Some(7))?;
    value["reference"]["resource_id"] = json!("zz-unreferenced");
    Ok(value)
}
async fn operation(
    client: &TenancyClient,
    installation: &InstallationId,
    caller: &PrincipalId,
    id: &OperationId,
    stream: &mut JsonLines,
    revision: &mut u64,
) -> Result<OperationResult> {
    let result = client.operation(installation, id).await?;
    let next = revision
        .checked_add(1)
        .context("fixture revision overflow")?;
    ensure!(
        result.status == OperationStatus::Applied
            && result.authority_revision.context("applied revision")?.get() == next,
        "actual retained operation must be contiguous and Applied"
    );
    stream.record(&ProvisioningRecord::Operation {
        principal_id: caller.clone(),
        result: result.clone(),
    })?;
    *revision = next;
    Ok(result)
}
async fn released(path: &Path) -> Result<()> {
    let owner = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(owner) = crate::tenancy::ownership::acquire_path(path, false) {
                break owner;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("fresh-target worker cleanup not acknowledged")?;
    Source::inspect(path)?;
    drop(owner);
    Ok(())
}

struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    source: PathBuf,
    target: PathBuf,
    bundle: PathBuf,
    provisioning: Anchor,
    approval: Anchor,
    installation: InstallationId,
    plan: Value,
    stream: FileWitness,
}
impl Fixture {
    async fn new(work_deadline: tokio::time::Instant) -> Result<Self> {
        crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?;
        let directory = home()?;
        let home = directory.path().canonicalize()?;
        large::probe(&home, 4 * 1024 * 1024 * 1024, 0)?;
        let source = home.join("source.sqlite");
        let live = producer::Live::open(&source, None, None).await?;
        let old = producer::provision(&live).await;
        let closed = live.close(&source).await;
        closed?;
        let old = old?;
        let target = home.join("target.sqlite");
        let live = producer::Live::open(&target, None, Some(producer::KEY_ENV)).await?;
        let produced = tokio::time::timeout_at(work_deadline, async {
            let client = live.operator()?;
            let capabilities = client.capabilities().await?;
            ensure!(!capabilities.dispatch_enabled && !capabilities.strict_receipts && !capabilities.revision_bound_dispatch,
                "recovery producer must not expose inference dispatch");
            let installation = capabilities.installation_id;
            let caller = PrincipalId::for_control_credential("recovery-operator")?;
            let mut operations = JsonLines::create(&home.join("provisioning.jsonl"))?;
            operations.record(&ProvisioningHeader { format: format::PROVISIONING.into(), installation_id: installation.clone(),
                operation_count: OPERATIONS, configuration_count: TENANTS, active_count: TENANTS, resource_count: RESOURCES })?;
            let expires = (Utc::now() + chrono::Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let mut revision = 0;
            let mut issued_count = 0;
            for index in 0..TENANTS {
                let tenant: TenantId = tenant(index).try_into()?;
                let principal: PrincipalId = principal(index).try_into()?;
                let id: OperationId = format!("tenant-{index:02}").try_into()?;
                client.put_tenant(&tenant, &TenantPut { operation_id: id.clone(), expected_revision: None,
                    state: TenantState::Active, installation_id: installation.clone() }).await?;
                operation(&client,&installation,&caller,&id,&mut operations,&mut revision).await?;
                let id: OperationId = format!("principal-{index:02}").try_into()?;
                client.put_principal(&principal,&PrincipalPut { operation_id:id.clone(), expected_revision:None,
                    issuer:"scale-owner".into(),subject:principal.to_string(),installation_id:installation.clone() }).await?;
                operation(&client,&installation,&caller,&id,&mut operations,&mut revision).await?;
                let id: OperationId = format!("binding-{index:02}").try_into()?;
                client.put_binding(&tenant,&"binding".try_into()?,&BindingPut { operation_id:id.clone(), expected_revision:None,
                    principal_id:principal.clone(), role:TenantRole::TenantAdmin, scope:GrantScope::Tenant{operator_pools:vec![]},
                    expires_at:expires.as_str().try_into()?,expected_management_owner:None,installation_id:installation.clone() }).await?;
                operation(&client,&installation,&caller,&id,&mut operations,&mut revision).await?;
                for key in 0..if index < 26 {8} else {7} {
                    let id: OperationId = format!("credential-{index:02}-{key}").try_into()?;
                    let issued = client.issue_credential(&tenant,&CredentialCreate { operation_id:id.clone(),expected_authority_revision:revision.try_into()?,
                        principal_id:principal.clone(),permissions:vec![Permission::ResourceRead,Permission::ResourceDraft,Permission::PolicyActivate],
                        scope:GrantScope::Tenant{operator_pools:vec![]},expires_at:expires.as_str().try_into()?,rotate_credential_id:None,
                        binding_id:"binding".try_into()?,installation_id:installation.clone() }).await?;
                    let retained = client.credential(&installation,&tenant,&issued.credential_id).await?;
                    ensure!(retained.credential_id == issued.credential_id && retained.state == CredentialState::Active);
                    operation(&client,&installation,&caller,&id,&mut operations,&mut revision).await?;
                    if key == 0 { producer::private_json(&home.join(format!("secret-{index:02}.json")),&json!({"secret":issued.secret.context("one-time fixture credential")?}))?; }
                    issued_count += 1;
                }
            }
            ensure!(issued_count == CREDENTIALS && revision == 356);
            Ok::<_,anyhow::Error>((installation,operations,revision))
        }).await;
        let closed = live.close(&target).await;
        closed?;
        let (installation, mut operations, mut revision) = produced??;
        // Two complete-tenant imports stay inside the existing per-import cap.
        for (batch, (first, end)) in [(0, 16), (16, 33)].into_iter().enumerate() {
            let mut resources = Vec::new();
            for index in first..end {
                resources.push(resource(index, None)?);
                for route in 0..8 {
                    resources.push(resource(index, Some(route))?);
                }
            }
            ensure!(resources.len() <= 153);
            let import = home.join(format!("import-{batch}.json"));
            producer::private_json(
                &import,
                &json!({"format":1,"installation_id":installation,"resources":resources}),
            )?;
            ensure!(witness(&import)?.bytes <= 4 * 1024 * 1024);
            drop(resources);
            large::probe(&home, FLOOR, witness(&target)?.bytes)?;
            let live =
                producer::Live::open(&target, Some(&import), Some(producer::KEY_ENV)).await?;
            live.close(&target).await?;
        }
        let mut configurations = JsonLines::create(&home.join("configurations.jsonl"))?;
        let mut active = JsonLines::create(&home.join("active.jsonl"))?;
        let live = producer::Live::open(&target, None, Some(producer::KEY_ENV)).await?;
        let configured = tokio::time::timeout_at(work_deadline, async {
            for index in 0..TENANTS {
                let client = tenant_client(&live, &home, index)?;
                let tenant: TenantId = tenant(index).try_into()?;
                let caller: PrincipalId = principal(index).try_into()?;
                let id: OperationId = format!("configuration-{index:02}").try_into()?;
                let prepared = client
                    .put_configuration(
                        &tenant,
                        &ConfigurationPut {
                            operation_id: id.clone(),
                            expected_authority_revision: revision.try_into()?,
                            document: document(index)?,
                            installation_id: installation.clone(),
                        },
                    )
                    .await?;
                operation(
                    &client,
                    &installation,
                    &caller,
                    &id,
                    &mut operations,
                    &mut revision,
                )
                .await?;
                let retained = client
                    .configuration(&installation, &tenant, &prepared.revision_id)
                    .await?;
                ensure!(serde_json::to_value(&retained)? == serde_json::to_value(&prepared)?);
                configurations.record(&ProvisioningRecord::Configuration { revision: retained })?;
                let id: OperationId = format!("activate-{index:02}").try_into()?;
                let activated = client
                    .activate(
                        &tenant,
                        &Activation {
                            operation_id: id.clone(),
                            expected_active_revision: None,
                            revision_id: prepared.revision_id.clone(),
                            expected_authority_revision: revision.try_into()?,
                            installation_id: installation.clone(),
                        },
                    )
                    .await?;
                operation(
                    &client,
                    &installation,
                    &caller,
                    &id,
                    &mut operations,
                    &mut revision,
                )
                .await?;
                let retained = client.active_revision(&installation, &tenant).await?;
                ensure!(retained.revision_id.as_ref() == Some(&prepared.revision_id));
                active.record(&ProvisioningRecord::Active {
                    tenant_id: tenant,
                    revision_id: prepared.revision_id,
                    authority_revision: activated
                        .authority_revision
                        .context("activation revision")?,
                })?;
            }
            ensure!(revision == 422);
            let last = append_principals(
                &live.operator()?,
                &installation,
                &PrincipalId::for_control_credential("recovery-operator")?,
                0,
                TAIL,
                &mut operations,
            )
            .await?;
            ensure!(last.get() == OPERATIONS);
            Ok::<_, anyhow::Error>(())
        })
        .await;
        let closed = live.close(&target).await;
        closed?;
        configured??;
        let (configurations, _) = configurations.finish()?;
        let (active, _) = active.finish()?;
        operations.append_section(&configurations.path)?;
        operations.append_section(&active.path)?;
        let mut approval = JsonLines::create(&home.join("approval.jsonl"))?;
        approval.record(&ApprovalHeader {
            format: format::APPROVAL.into(),
            installation_id: installation.clone(),
            resource_count: RESOURCES,
        })?;
        for index in 0..TENANTS {
            for route in std::iter::once(None).chain((0..8).map(Some)) {
                operations.record(&ProvisioningRecord::Resource {
                    reference: reference(index, route)?,
                })?;
                let mut approved = resource(index, route)?;
                approved["kind"] = json!("resource");
                approval.record(&approved)?;
            }
        }
        let (provisioning, stream) = operations.finish()?;
        ensure!(
            stream.bytes > 2 * 1024 * 1024,
            "legitimate provisioning stream did not cross 2 MiB"
        );
        let (approval, _) = approval.finish()?;
        let bundle = home.join("bundle");
        large::probe(&home, FLOOR, witness(&source)?.bytes)?;
        let evidence = {
            let report =
                crate::offline::snapshot_with_options(&source, &bundle, settings(&home)).await?;
            serde_json::to_value(&report as &dyn crate::output::CliReport)?
        };
        let commit = crate::artifact::current()
            .source_revision
            .context("real producer revision")?;
        let executable = witness(&std::env::current_exe()?)?;
        let attestation = producer::private_json(
            &home.join("producer.json"),
            &json!({"format":"bitrouter.offline.source-producer-attestation.v1",
            "installation_id":old.installation,"commit":commit,"artifact_sha256":executable.sha256,"database_evidence_digest":evidence["evidence_digest"]}),
        )?;
        let mut retirement = JsonLines::create(&home.join("retirement.jsonl"))?;
        retirement.record(&json!({"format":format::RETIREMENT,"source_installation_id":old.installation,"database_evidence_digest":evidence["evidence_digest"],
            "fence_maintained":true,"target_ingress_fenced":true,"selection_exhaustive":true,"unresolved_effects_remain_unknown":true,"writer_count":1,"selected_file_count":0}))?;
        retirement.record(&json!({"kind":"writer","id":"scale-source","process_identity":format!("Service:{}",std::process::id()),"restart_source":"none","retired":true}))?;
        let (retirement, _) = retirement.finish()?;
        let tables = evidence["evidence"]["inventory"]["tables"].as_array().context("source schema tables")?.iter().map(|table|
            json!({"table":table["name"],"rows":table["rows"],"value_bytes":table["value_bytes"],"digest":table["digest"],"disposition":"retained_archive"})).collect::<Vec<_>>();
        let plan = json!({"format":format::PLAN,"source":{"bundle":bundle,"evidence_digest":evidence["evidence_digest"],"installation_id":old.installation,
            "producer":{"commit":commit,"artifact_sha256":executable.sha256,"evidence":attestation},"tables":tables},
            "target":{"database":target,"installation_id":installation,"provisioning":provisioning,"wrapping_key_env":producer::KEY_ENV,"static_manifest":approval},"retirement":retirement});
        Ok(Self {
            _directory: directory,
            home,
            source,
            target,
            bundle,
            provisioning,
            approval,
            installation,
            plan,
            stream,
        })
    }
}

fn tenant_client(live: &producer::Live, home: &Path, index: u64) -> Result<TenancyClient> {
    let path = home.join(format!("secret-{index:02}.json"));
    ensure!(std::fs::metadata(&path)?.len() < 4096);
    let secret: Value = serde_json::from_reader(File::open(path)?)?;
    Ok(TenancyClient::new(
        &live.base,
        secret["secret"]
            .as_str()
            .context("private fixture credential")?,
    )?)
}

async fn counts(path: &Path) -> Result<Value> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let path = owner.path.clone();
    let mut connections = Connections::new(CancellationToken::new(), Some(owner));
    let result = async {
        connections.open_immutable_source(&path).await?;
        let db = connections.source.as_mut().context("counts connection")?;
        let mut counts = serde_json::Map::new();
        for table in ["tenancy_tenants","tenancy_principals","tenancy_bindings","tenancy_credentials","tenancy_configurations","tenancy_active_configurations","tenancy_resource_revisions","tenancy_operations"] {
            let count: i64 = sea_orm::sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(&mut *db).await?;
            counts.insert(table.into(),json!(count));
        }
        let row = sea_orm::sqlx::query("SELECT authority_revision,operation_ids,safety_operation_ids,saturated FROM tenancy_installation WHERE singleton=1").fetch_one(&mut *db).await?;
        for column in ["authority_revision","operation_ids","safety_operation_ids","saturated"] { counts.insert(column.into(),json!(row.try_get::<i64,_>(column)?)); }
        let applied: i64 = sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM tenancy_operations WHERE json_extract(result_json,'$.status')='applied'").fetch_one(&mut *db).await?;
        counts.insert("applied".into(),json!(applied));
        Ok::<_,anyhow::Error>(Value::Object(counts))
    }.await;
    let closed = connections.close().await;
    drop(connections);
    closed?;
    released(&path).await?;
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Negative {
    Malformed,
    Duplicate,
    Missing,
    Revision,
    Extra,
    Orphan,
    CrossSection,
}
impl Negative {
    fn label(self) -> &'static str {
        match self {
            Self::Malformed => "malformed-final",
            Self::Duplicate => "global-operation-id",
            Self::Missing => "missing-last-operation",
            Self::Revision => "wrong-revision",
            Self::Extra => "extra-retained-resource",
            Self::Orphan => "orphan-approved-dependency",
            Self::CrossSection => "cross-section",
        }
    }
    fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "invalid offline JSON-lines record",
            Self::Duplicate => "duplicate global provisioning operation ID",
            Self::Missing => "wrong provisioning section",
            Self::Revision => "noncontiguous provisioning operation revision",
            Self::Extra => "fresh readback coverage mismatch",
            Self::Orphan => "unreferenced approved resource",
            Self::CrossSection => "approval/readback resource mismatch",
        }
    }
}
struct Duplicate {
    operation: Value,
    configuration: Value,
}

async fn duplicate(
    fixture: &Fixture,
    path: &Path,
    work_deadline: tokio::time::Instant,
) -> Result<Duplicate> {
    let live = producer::Live::open(path, None, Some(producer::KEY_ENV)).await?;
    let result = tokio::time::timeout_at(work_deadline, async {
        let client = tenant_client(&live, &fixture.home, 0)?;
        // The operator used this ID for tenant creation; this different actual
        // principal can retain it, but the recovery stream must reject globally.
        let id: OperationId = "tenant-00".try_into()?;
        let revision = client
            .put_configuration(
                &tenant(0).try_into()?,
                &ConfigurationPut {
                    operation_id: id.clone(),
                    expected_authority_revision: OPERATIONS.try_into()?,
                    document: document(0)?,
                    installation_id: fixture.installation.clone(),
                },
            )
            .await?;
        let result = client.operation(&fixture.installation, &id).await?;
        ensure!(
            result.status == OperationStatus::Applied
                && result
                    .authority_revision
                    .context("duplicate actual revision")?
                    .get()
                    == OPERATIONS + 1
        );
        Ok::<_, anyhow::Error>(Duplicate {
            operation: serde_json::to_value(ProvisioningRecord::Operation {
                principal_id: principal(0).try_into()?,
                result,
            })?,
            configuration: serde_json::to_value(ProvisioningRecord::Configuration { revision })?,
        })
    })
    .await;
    let closed = live.close(path).await;
    closed?;
    result?
}

fn transform(
    source: &Path,
    destination: &Path,
    negative: Negative,
    approval: bool,
    duplicate: Option<&Duplicate>,
) -> Result<(Anchor, usize)> {
    let total = if approval {
        1 + RESOURCES
    } else {
        1 + OPERATIONS + TENANTS * 2 + RESOURCES
    };
    let mut input = BufReader::with_capacity(BUFFER, File::open(source)?);
    let mut output = JsonLines::create(destination)?;
    let mut line = Vec::new();
    let mut maximum = 0;
    for index in 0..total {
        line.clear();
        (&mut input).take(RECORD + 1).read_until(b'\n', &mut line)?;
        ensure!(
            !line.is_empty() && u64::try_from(line.len())? <= RECORD && line.last() == Some(&b'\n'),
            "fixture record exceeds individual profile"
        );
        maximum = maximum.max(line.len());
        let mut value: Value = serde_json::from_slice(&line)?;
        if index == 0 {
            if negative == Negative::Orphan {
                value["resource_count"] = json!(RESOURCES + 1);
            }
            if !approval && negative == Negative::Duplicate {
                value["operation_count"] = json!(OPERATIONS + 1);
                value["configuration_count"] = json!(TENANTS + 1);
            }
        }
        if !approval && index == OPERATIONS {
            if negative == Negative::Missing {
                continue;
            }
            if negative == Negative::Revision {
                value["result"]["authority_revision"] = json!(OPERATIONS - 1);
            }
        }
        if !approval && negative == Negative::Malformed && index == total - 1 {
            value = json!({});
        }
        if approval && negative == Negative::CrossSection && index == total - 1 {
            value["reference"]["resource_id"] = json!("zz-cross-section");
        }
        if !approval && negative == Negative::Duplicate && index == OPERATIONS + 1 {
            let extra = &duplicate
                .context("real duplicate operation fixture")?
                .configuration;
            let first = value["revision"]["revision_id"]
                .as_str()
                .context("configuration revision")?;
            let second = extra["revision"]["revision_id"]
                .as_str()
                .context("second configuration revision")?;
            ensure!(first != second);
            if first < second {
                output.record(&value)?;
                output.record(extra)?;
            } else {
                output.record(extra)?;
                output.record(&value)?;
            }
        } else {
            output.record(&value)?;
        }
        if !approval && negative == Negative::Duplicate && index == OPERATIONS {
            output.record(&duplicate.context("duplicate")?.operation)?;
        }
    }
    ensure!(input.fill_buf()?.is_empty(), "unexpected source record");
    if negative == Negative::Orphan {
        let mut extra = extra_resource()?;
        if !approval {
            extra
                .as_object_mut()
                .context("extra reference")?
                .remove("definition");
        }
        extra["kind"] = json!("resource");
        output.record(&extra)?;
    }
    Ok((output.finish()?.0, maximum))
}

fn copy_database(source: &Path, destination: &Path) -> Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let mut buffer = [0_u8; BUFFER];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
    }
    output.sync_all()?;
    drop(output);
    ensure!(witness(source)? == witness(destination)?);
    Ok(())
}

async fn reject(
    fixture: &Fixture,
    negative: Negative,
    work_deadline: tokio::time::Instant,
    observation: &Observation,
) -> Result<usize> {
    let directory = fixture.home.join(negative.label());
    std::fs::create_dir(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let target = directory.join("target.sqlite");
    large::probe(&directory, FLOOR, witness(&fixture.target)?.bytes)?;
    copy_database(&fixture.target, &target)?;
    let duplicate = if negative == Negative::Duplicate {
        Some(duplicate(fixture, &target, work_deadline).await?)
    } else {
        None
    };
    if matches!(negative, Negative::Extra | Negative::Orphan) {
        let manifest = directory.join("extra-import.json");
        producer::private_json(
            &manifest,
            &json!({"format":1,"installation_id":fixture.installation,"resources":[extra_resource()?]}),
        )?;
        let live = producer::Live::open(&target, Some(&manifest), Some(producer::KEY_ENV)).await?;
        live.close(&target).await?;
    }
    let (provisioning, maximum) = transform(
        &fixture.provisioning.path,
        &directory.join("provisioning.jsonl"),
        negative,
        false,
        duplicate.as_ref(),
    )?;
    let (approval, approval_maximum) = transform(
        &fixture.approval.path,
        &directory.join("approval.jsonl"),
        negative,
        true,
        duplicate.as_ref(),
    )?;
    let mut plan = fixture.plan.clone();
    plan["target"]["database"] = json!(target);
    plan["target"]["provisioning"] = serde_json::to_value(provisioning)?;
    plan["target"]["static_manifest"] = serde_json::to_value(approval)?;
    let plan_path = directory.join("plan.json");
    producer::private_json(&plan_path, &plan)?;
    // Compare against this variant's post-setup bytes, not the pristine target.
    released(&target).await?;
    let before = witness(&target)?;
    let output = directory.join("refused");
    large::probe(&directory, FLOOR, before.bytes)?;
    let result = tokio::time::timeout_at(
        work_deadline,
        crate::offline::recovery::run_with_options(
            &plan_path,
            &output,
            ValidationBudget::default(),
            settings(&fixture.home),
        ),
    )
    .await;
    released(&target).await?;
    observation.sample();
    let error = result?.err().context("negative large proof succeeded")?;
    ensure!(
        format!("{error:#}").contains(negative.reason()),
        "{} failed at wrong stage: {error:#}",
        negative.label()
    );
    ensure!(!output.join("VERIFIED").exists() && !output.join("report.json").exists());
    ensure!(
        witness(&target)? == before,
        "verifier changed the negative target"
    );
    eprintln!(
        "{}",
        json!({"qualification":"S-negative","case":negative.label(),"expected_stage":negative.reason(),
        "target_bytes":before.bytes,"target_sha256":before.sha256,"owner_reacquired":true})
    );
    observation.sample();
    std::fs::remove_dir_all(directory)?;
    observation.sample();
    Ok(maximum.max(approval_maximum))
}

#[tokio::test]
#[ignore = "explicit offline scale qualification; real SDK fresh target, 300-second gate"]
async fn large_fresh_target_streams_complete_sections_and_rejects_late_mismatch() -> Result<()> {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(300);
    let work_deadline = deadline - Duration::from_secs(15);
    let observation = Observation::new(&std::env::temp_dir().canonicalize()?)?;
    // Retain a constructed fixture through errors/timeouts until the pre-deletion sample.
    let mut fixture = None;
    let result = {
        let work = async {
            tokio::time::timeout_at(deadline,async {
        let fixture = fixture.insert(Fixture::new(work_deadline).await?);
        let observed = counts(&fixture.target).await?;
        for (name,count) in [("tenancy_tenants",TENANTS),("tenancy_principals",TENANTS+TAIL),("tenancy_bindings",TENANTS),
            ("tenancy_credentials",CREDENTIALS),("tenancy_configurations",TENANTS),("tenancy_active_configurations",TENANTS),
            ("tenancy_resource_revisions",RESOURCES),("tenancy_operations",OPERATIONS),("authority_revision",OPERATIONS),("operation_ids",OPERATIONS),("applied",OPERATIONS)] {
            ensure!(observed[name] == json!(count),"incorrect actual count for {name}");
        }
        ensure!(observed["safety_operation_ids"] == 0 && observed["saturated"] == 0);
        let source = witness(&fixture.source)?;
        let archive = bundle_witness(&fixture.bundle)?;
        let target = witness(&fixture.target)?;
        let path = fixture.home.join("positive-plan.json");
        producer::private_json(&path,&fixture.plan)?;
        let output = fixture.home.join("verified");
        large::probe(&fixture.home,FLOOR,target.bytes)?;
        let verified = tokio::time::timeout_at(work_deadline,crate::offline::recovery::run_with_options(
            &path,&output,ValidationBudget::default(),settings(&fixture.home))).await;
        released(&fixture.target).await?;
        observation.sample();
        let report = verified??;
        let value = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
        ensure!(value["target"]["installation_id"] == fixture.plan["target"]["installation_id"]
            && value["source"]["installation_id"] == fixture.plan["source"]["installation_id"]
            && value["source"]["ordinary_operations"] == 4 && value["source"]["authority_revision"] == 4
            && value["target"]["ordinary_operations"] == OPERATIONS && value["target"]["authority_revision"] == OPERATIONS
            && value["fresh_provisioning_verified"] == true && value["static_closure_authenticated"] == true
            && value["operational_fence_attestation_bound"] == true && value["source_producer_attestation_bound"] == true
            && value["history_transferred"] == false && value["cutover_ready"] == false && value["complete"] == false);
        let published = witness(&output.join("report.json"))?;
        ensure!(std::fs::read_to_string(output.join("VERIFIED"))? == format!("{}\n",published.sha256));
        ensure!(!output.join("INCOMPLETE").exists() && witness(&fixture.target)? == target);
        let mut maximum = 0;
        for negative in [Negative::Malformed,Negative::Duplicate,Negative::Missing,Negative::Revision,Negative::Extra,Negative::Orphan,Negative::CrossSection] {
            maximum = maximum.max(reject(fixture,negative,work_deadline,&observation).await?);
            ensure!(witness(&fixture.source)? == source && bundle_witness(&fixture.bundle)? == archive && witness(&fixture.target)? == target);
        }
        ensure!(counts(&fixture.target).await? == observed);
        eprintln!("{}",json!({"qualification":"S","artifact":crate::artifact::current(),"test_executable_sha256":witness(&std::env::current_exe()?)?.sha256,
            "counts":observed,"provisioning_bytes":fixture.stream.bytes,"provisioning_sha256":fixture.stream.sha256,
            "negative_cases":7,"file_buffer_bytes":BUFFER,"maximum_record_bytes":maximum,"maximum_import_records":153,
            "working_bytes":settings(&fixture.home).working_bytes,"free_floor":FLOOR,"elapsed_ms":started.elapsed().as_millis(),
            "rss":"external exact-test process measurement required","cleanup":"actual close and owner reacquisition"}));
        Ok::<_,anyhow::Error>(())
            }).await?
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = observation.tick() => {}
            }
        }
    };
    observation.sample();
    drop(fixture);
    observation.sample();
    let measured = observation.finish("S:parent", result.is_ok(), false);
    result?;
    measured?;
    ensure!(
        tokio::time::Instant::now() <= deadline,
        "S qualification exceeded 300 seconds including cleanup and observations"
    );
    Ok(())
}
