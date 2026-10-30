//! Actual unsupported Generation/count consumers and released V1 compatibility.
use super::*;

#[tokio::test]
#[ignore = "real unsupported native work and released-V1 compatibility; select this parent"]
async fn native_recovery_refuses_uncovered_generation_and_auxiliary_but_reopens_released_v1()
-> Result<()> {
    campaign(&[
        "profile-generation",
        "profile-count",
        "profile-oauth",
        "compat-v1-released",
    ])
    .await
}

pub(super) async fn case(root: &Path, mode: &str) -> Result<()> {
    let mut generation = Provider::start(mode != "compat-v1-released").await?;
    let mut choice = wire::Endpoint::start(false, false).await?;
    std::fs::create_dir(root.join("workspace"))?;
    let mut initial = if mode == "profile-count" {
        let (fixture, route) =
            crate::tenancy::tests::execution::native::session::file_fixture_profile(
                &generation,
                root,
                true,
            )
            .await?;
        let revision = draft(&fixture, &route, "count-recovery-profile", 25).await?;
        let key = fixture.issue("count-recovery-executor").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = crate::tenancy::tests::execution::headers(
            &fixture,
            key.secret.as_deref().context("actual count credential")?,
            &revision.revision_id,
        )?;
        Bound {
            fixture,
            route,
            key,
            headers,
            revision: revision.revision_id,
        }
    } else {
        fixture::sqlite(&generation, &choice, root, 4).await?
    };
    if mode == "profile-oauth" {
        oauth_profile(&mut initial, &generation).await?;
    }
    let spec = Spec::capture(&initial, &generation)?;
    private_json(&root.join("spec.json"), &spec)?;
    let released = if mode == "compat-v1-released" {
        // This is the ordinary raw fixture Service, which has no StartupOwner.
        // Its actual native writer emits V1; no JSON is forged or converted.
        let session = fixture::open(
            &initial,
            &generation,
            &root.join("workspace"),
            Arc::new(HostSpies::default()),
        )
        .await?;
        session.close().await?;
        let bytes = payload(&initial.fixture.db).await?;
        let value: Value = serde_json::from_str(&bytes)?;
        ensure!(
            value["format"] == 1
                && value["recovery"].is_null()
                && value["journal"]["released"] == true
                && value["journal"]["ack"].is_object()
        );
        Some(bytes)
    } else {
        None
    };
    initial.fixture.service.shutdown().await?;
    initial.fixture.db.clone().close().await?;
    drop(initial);
    wait_file(root).await?;
    let observed = async {
        if mode != "compat-v1-released" {
            let mut producer = Child::start("native_stopped_owner_producer", root, mode)?;
            let sent = async {
                if mode == "profile-oauth" {
                    producer.line("native-unsupported-open").await?;
                    ensure!(generation.calls() == 0 && choice.calls() == 0);
                    locked(root)?;
                    return Ok::<_, anyhow::Error>(());
                }
                producer.line("native-uncovered-running").await?;
                let request = generation.receive().await?;
                ensure!(
                    request.path
                        == if mode == "profile-count" {
                            "/v1/responses/input_tokens"
                        } else {
                            "/v1/chat/completions"
                        }
                );
                ensure!(
                    request
                        .headers
                        .get(http::header::AUTHORIZATION)
                        .is_some_and(|v| v == "Bearer owned-upstream-secret")
                );
                ensure!(generation.calls() == 1 && choice.calls() == 0);
                locked(root)?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let reaped = producer.reap(true).await;
            sent?;
            reaped?;
        }
        let bound = spec.connect(root).await?;
        let checked = async {
            let before = payload(&bound.fixture.db).await?;
            let terminals_before = terminals(&bound.fixture.db).await?;
            if let Some(released) = &released {
                ensure!(&before == released, "bootstrap rewrote released V1 bytes");
                ensure!(terminals_before.is_empty());
                let session = fixture::open(
                    &bound,
                    &generation,
                    &root.join("workspace"),
                    Arc::new(HostSpies::default()),
                )
                .await?;
                ensure!(session.core().snapshot().await.run.is_none());
                session.close().await?;
                let migrated: Value = serde_json::from_str(&payload(&bound.fixture.db).await?)?;
                let old: Value = serde_json::from_str(released)?;
                ensure!(migrated["format"] == 2 && migrated["recovery"]["phase"] == "released");
                ensure!(migrated["descriptor"] == old["descriptor"]);
                ensure!(terminals(&bound.fixture.db).await? == terminals_before);
                ensure!(generation.calls() == 0 && choice.calls() == 0);
            } else {
                let value: Value = serde_json::from_str(&before)?;
                ensure!(value["format"] == 2 && value["journal"]["released"] == false);
                if matches!(mode, "profile-count" | "profile-oauth") {
                    ensure!(
                        value["recovery"]["eligible"] == false,
                        "actual auxiliary route falsely marked recovery eligible"
                    );
                }
                let opened = fixture::open(
                    &bound,
                    &generation,
                    &root.join("workspace"),
                    Arc::new(HostSpies::default()),
                )
                .await;
                if let Ok(session) = opened {
                    session.close().await?;
                    anyhow::bail!("uncovered work acquired usable native handle");
                }
                ensure!(payload(&bound.fixture.db).await? == before);
                ensure!(terminals(&bound.fixture.db).await? == terminals_before);
                ensure!(
                    generation.calls() == usize::from(mode != "profile-oauth")
                        && choice.calls() == 0
                );
            }
            tokio::time::timeout(TIMEOUT, async {
                while bound.fixture.service.host_slots_for_test() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let shutdown = bound.fixture.service.shutdown().await;
        let pool = bound.fixture.db.clone().close().await;
        checked?;
        shutdown?;
        // Host-slot return does not quiesce H maintenance; the same Service drain does.
        bound.fixture.service.cleanup.ensure_clean()?;
        pool?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    choice.release();
    let choice_closed = choice.finish().await;
    let generation_closed = generation.finish().await;
    observed?;
    choice_closed?;
    generation_closed
}

async fn oauth_profile(bound: &mut Bound, provider: &Provider) -> Result<()> {
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: bound.fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "recovery-oauth-r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let slot = reference(ResourceKind::Credential, "recovery-oauth-slot")?;
    let registration = crate::gateway::providers::login::find("anthropic")
        .context("implemented OAuth registration")?;
    bound.fixture.service.resources.provision_bytes(&bound.fixture.db,
        &serde_json::to_vec(&json!({"format":1,"installation_id":bound.fixture.installation,"resources":[
            {"reference":slot,"definition":{"kind":"credential","credential":{"type":"oauth","data":{
                "access_token":"recovery-access","refresh_token":"recovery-refresh","expires_at":1}},
                "oauth":{"format":1,"mechanism":"claude_code_refresh_v1","provider":"claude-code","account":"default",
                    "registration":"anthropic","token_endpoint":format!("{}/oauth/token",provider.url),"client_id":registration.auth.client_id}}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,
                "routing":{"providers":{"claude-code":{"api_base":provider.url,"api_protocol":[{"*":"messages"}],"models":[{"id":"model-a"}]}},
                    "routers":{"owned":{"selection":{"kind":"model","model":"claude-code:model-a"}}}},
                "credentials":[{"provider":"claude-code","account":"default","credential":slot}]}}
        ]}))?).await?;
    let revision = draft(&bound.fixture, &route, "recovery-oauth-configuration", 25).await?;
    bound.headers = crate::tenancy::tests::execution::headers(
        &bound.fixture,
        bound.key.secret.as_deref().context("actual credential")?,
        &revision.revision_id,
    )?;
    bound.route = route;
    bound.revision = revision.revision_id;
    Ok(())
}
