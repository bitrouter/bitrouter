use anyhow::{Context, Result as TestResult, ensure};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::{Configuration, ResourceRef},
    },
    types::ApiProtocol,
};
use serde_json::json;

use super::super::super::{
    Envelope,
    manifest::{Artifact, Definition, Import},
};
use super::super::{RetainedExecutionRevision, RetainedRoute, retained};
use super::*;
use crate::gateway::{
    caller::CallerContext,
    pipeline::{
        routing::{RoutingPrefs, RoutingTable},
        types::RoutingTarget,
    },
};

mod bounds;
mod operation;
mod persistence;

#[tokio::test]
async fn registry_requirements_are_runtime_only_for_owned_routes() -> TestResult<()> {
    let prefs = RoutingPrefs {
        registry_requirements: Some(Default::default()),
        ..Default::default()
    };
    ensure!(Budget::new().prefs(&RoutingPrefs::default()).is_ok());
    ensure!(matches!(
        Budget::new().prefs(&prefs),
        Err(Unavailable::Unsupported)
    ));
    let view = view(envelope(&basic("visible")?, None)?)?;
    let route = checked(view.select_native("visible", ModelOperation::Generation))?;
    let targets = route
        .route_chain("visible", &prefs, &CallerContext::local())
        .await?;
    ensure!(!targets.is_empty());
    ensure!(targets.iter().all(|target| target.registry.is_none()));
    Ok(())
}

fn checked<T>(result: std::result::Result<T, ErrorCode>) -> TestResult<T> {
    result.map_err(|code| anyhow::anyhow!("unexpected native selection error: {code:?}"))
}

fn reference(kind: ResourceKind, name: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant".try_into()?,
        resource_kind: kind,
        resource_id: name.try_into()?,
        revision_id: "v1".try_into()?,
    })
}

fn document(routes: Vec<ResourceRef>) -> TestResult<Configuration> {
    Ok(serde_json::from_value(json!({
        "routes": routes, "policy": null,
        "generation_defaults": {"max_output_tokens":128,"temperature":0.5},
        "learning": {"mode":"off","max_evaluations":0},
        "limits": {"active_executions":1,"queued_work":1,"learning_jobs":0}
    }))?)
}

fn input(
    name: &str,
    selector: &str,
    source: Value,
    slots: &[(&str, &str, &str)],
) -> TestResult<Import> {
    let credentials = slots
        .iter()
        .map(|(provider, account, slot)| {
            Ok(CredentialBinding {
                provider: (*provider).into(),
                account: (*account).into(),
                credential: reference(ResourceKind::Credential, slot)?,
            })
        })
        .collect::<TestResult<Vec<_>>>()?;
    Ok(Import {
        reference: reference(ResourceKind::Route, name)?,
        definition: Definition::Route {
            assessment: None,
            block: None,
            routing: source,
            selector: selector.into(),
            policy: None,
            credentials,
            exploration: Vec::new(),
            core_choice: None,
        },
    })
}

fn basic(name: &str) -> TestResult<Import> {
    input(
        name,
        "fixture:model",
        json!({"providers": {"fixture": {
            "api_base":"https://example.com/v1", "models":[{"id":"model"}],
            "api_protocol":[{"*":["messages","chat_completions","responses"]}]
        }}}),
        &[("fixture", "default", "slot")],
    )
}

fn envelope(input: &Import, policy: Option<Artifact>) -> TestResult<Envelope> {
    input.validate()?;
    let route = compiler::prepare_route(input, policy.as_ref())?;
    let mut artifacts = vec![Artifact {
        reference: input.reference.clone(),
        definition: input.definition.clone(),
        envelope: Some(route.clone()),
    }];
    artifacts.extend(policy);
    let mut document = document(vec![input.reference.clone()])?;
    document.policy = input.definition.policy_reference().cloned();
    let routes = checked(compiler::configuration_routes(&document, &artifacts))?;
    Ok(Envelope {
        format: Envelope::format(&artifacts),
        document,
        routes,
        artifacts,
    })
}

fn view(envelope: Envelope) -> TestResult<RetainedExecutionRevision> {
    checked(RetainedExecutionRevision::new(
        "installation".try_into()?,
        "tenant".try_into()?,
        "r1".try_into()?,
        1.try_into()?,
        retained::decode(envelope)?,
    ))
}

async fn targets(route: &RetainedRoute, model: &str) -> TestResult<Vec<RoutingTarget>> {
    Ok(route
        .route_chain(model, &RoutingPrefs::default(), &CallerContext::local())
        .await?)
}

fn mixed() -> TestResult<Import> {
    let mut input = input(
        "literal:fast",
        "shared",
        json!({
            "providers": {
                "alpha": {
                    "api_base":"https://alpha.example.com/v1",
                    "api_protocol":[{"*":["chat_completions","responses","messages"]}],
                    "models":[{"id":"shared","provider_model_id":"alpha-upstream",
                        "api_protocol":["messages","responses","chat_completions"]}],
                    "protocol_endpoints":{"messages":"https://alpha.example.com/messages"},
                    "account_strategy":"balance", "accounts":[
                        {"label":"primary","api_base":"https://account.example.com/v1"},
                        {"label":"backup"}
                    ],
                    "headers":{"x-b":"second","x-a":"${HOME}"}
                },
                "middle": {
                    "api_base":"https://middle.example.com/v1", "models":[{"id":"shared"}]
                },
                "zeta": {
                    "api_base":"https://zeta.example.com/v1",
                    "api_protocol":[{"*":["messages","chat_completions","responses"]},
                        {"sha*":["responses","messages","chat_completions"]}],
                    "protocol_endpoints":{"responses":"https://zeta.example.com/responses"},
                    "models":[{"id":"shared","provider_model_id":"zeta-upstream"}]
                }
            },
            "models": {
                "ordered":{"strategy":"priority","endpoints":[
                    {"provider":"zeta","service_id":"shared"},
                    {"provider":"middle","service_id":"shared"},
                    {"provider":"alpha","service_id":"shared"}
                ]},
                "cascade":{"strategy":"cascade","endpoints":[
                    {"provider":"zeta","service_id":"shared"},
                    {"provider":"middle","service_id":"shared"},
                    {"provider":"alpha","service_id":"shared"}
                ]}
            }
        }),
        &[
            ("alpha", "primary", "alpha-primary"),
            ("alpha", "backup", "alpha-backup"),
            ("middle", "default", "middle-slot"),
            ("zeta", "default", "zeta-slot"),
        ],
    )?;
    if let Definition::Route { exploration, .. } = &mut input.definition {
        *exploration = vec!["ordered".into(), "cascade".into()];
    }
    Ok(input)
}

#[tokio::test]
async fn canonical_chain_golden_keeps_heads_accounts_endpoints_models_and_wire_proofs_distinct()
-> TestResult<()> {
    let view = view(envelope(&mixed()?, None)?)?;
    let native = checked(view.select_native("literal:fast", ModelOperation::Generation))?;
    let resolved = native.resolve_model("literal:fast").await?;
    ensure!(resolved.prefs.inbound_protocol.is_none());
    let actual = targets(&native, "literal:fast").await?;
    let expected = [
        (
            "alpha",
            "alpha-upstream",
            ApiProtocol::Messages,
            Some("primary"),
            "https://account.example.com/v1",
            "alpha-primary",
        ),
        (
            "alpha",
            "alpha-upstream",
            ApiProtocol::Messages,
            Some("backup"),
            "https://alpha.example.com/messages",
            "alpha-backup",
        ),
        (
            "middle",
            "shared",
            ApiProtocol::ChatCompletions,
            None,
            "https://middle.example.com/v1",
            "middle-slot",
        ),
        (
            "zeta",
            "zeta-upstream",
            ApiProtocol::Responses,
            None,
            "https://zeta.example.com/responses",
            "zeta-slot",
        ),
    ];
    ensure!(actual.len() == expected.len());
    for (target, (provider, model, protocol, account, endpoint, credential)) in
        actual.iter().zip(expected)
    {
        assert_eq!(
            (&*target.provider_name, &*target.service_id),
            (provider, model)
        );
        assert_eq!(target.api_protocol, protocol);
        assert_eq!(target.account_label.as_deref(), account);
        assert_eq!(target.api_base, endpoint);
        assert_eq!(
            checked(native.match_candidate(target))?,
            reference(ResourceKind::Credential, credential)?
        );
        ensure!(
            target.api_key.is_empty()
                && target.api_key_override.is_none()
                && target.api_base_override.is_none()
        );
        assert_eq!(
            native.canonical_model_id("literal:fast", target).as_deref(),
            Some("literal:fast")
        );
    }
    let headers = &actual.first().context("alpha target")?.headers;
    let names = headers
        .iter()
        .map(|header| header.name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["x-a", "x-b"]);
    assert_eq!(
        headers
            .first()
            .context("header")?
            .default()
            .context("value")?
            .to_str()?,
        "${HOME}"
    );
    let caller = CallerContext::local();
    for (selector, expected) in [
        ("ordered", vec!["zeta", "middle", "alpha", "alpha"]),
        ("cascade", vec!["alpha", "alpha", "middle", "zeta"]),
    ] {
        let chain = native
            .route_resolved(selector, &RoutingPrefs::default(), &caller)
            .await?;
        assert_eq!(
            chain
                .iter()
                .map(|t| t.provider_name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    for inbound in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
    ] {
        let wire = checked(view.select("literal:fast", &inbound))?;
        let prefs = RoutingPrefs {
            inbound_protocol: Some(inbound.clone()),
            ..Default::default()
        };
        let chain = wire.route_chain("literal:fast", &prefs, &caller).await?;
        let alpha = chain.first().context("wire alpha")?;
        assert_eq!(alpha.api_protocol, inbound);
        if inbound != ApiProtocol::Messages {
            ensure!(matches!(
                native.match_candidate(alpha),
                Err(ErrorCode::Forbidden)
            ));
            ensure!(matches!(
                native.candidate_key(alpha),
                Err(ErrorCode::Forbidden)
            ));
        }
        ensure!(
            native
                .route_chain("literal:fast", &prefs, &caller)
                .await
                .is_err()
        );
        ensure!(
            native
                .route_resolved("shared", &prefs, &caller)
                .await
                .is_err()
        );
    }
    let mut changed = actual.first().context("target")?.clone();
    changed.headers.reverse();
    ensure!(matches!(
        native.match_candidate(&changed),
        Err(ErrorCode::Forbidden)
    ));
    let mut changed = actual.first().context("target")?.clone();
    changed.chat_supports_store = Some(false);
    ensure!(matches!(
        native.match_candidate(&changed),
        Err(ErrorCode::Forbidden)
    ));
    ensure!(matches!(
        view.select_native("literal", ModelOperation::Generation),
        Err(ErrorCode::NotFound)
    ));
    ensure!(matches!(
        view.select_native(" literal:fast", ModelOperation::Generation),
        Err(ErrorCode::NotFound)
    ));
    Ok(())
}

#[tokio::test]
async fn base_and_exploration_resolve_once_and_literal_name_can_equal_internal_selector()
-> TestResult<()> {
    let mut input = basic("fixture:fast:fast")?;
    if let Definition::Route {
        routing,
        selector,
        exploration,
        ..
    } = &mut input.definition
    {
        *selector = "bitrouter/fixed".into();
        *exploration = vec!["fixture:fast:fast".into()];
        routing["routers"] = json!({"fixed":{"selection":{"kind":"model","model":"fixture:fast"}}});
        routing["variants"] = json!({"fast":{}});
        routing["providers"]["fixture"]["models"] = json!([{"id":"fast"},{"id":"fixture"}]);
    }
    let view = view(envelope(&input, None)?)?;
    let native = checked(view.select_native("fixture:fast:fast", ModelOperation::Generation))?;
    assert_eq!(
        native.resolve_model("fixture:fast:fast").await?.clean_model,
        "bitrouter/fixed"
    );
    for chain in [
        targets(&native, "fixture:fast:fast").await?,
        native
            .route_resolved(
                "fixture:fast:fast",
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await?,
    ] {
        ensure!(chain.len() == 1);
        assert_eq!(chain.first().context("candidate")?.service_id, "fast");
    }
    Ok(())
}

#[tokio::test]
async fn same_selector_and_exact_credential_are_required_even_when_another_chain_approves_target()
-> TestResult<()> {
    let mut input = basic("literal")?;
    if let Definition::Route { exploration, .. } = &mut input.definition {
        exploration.push("fixture:other".into());
    }
    let original = envelope(&input, None)?;
    let mut changed: Envelope = serde_json::from_value(serde_json::to_value(&original)?)?;
    let route = changed.routes.first_mut().context("route")?;
    let base_target = route
        .chains
        .iter()
        .find(|c| c.selector == "fixture:model" && c.inbound == ApiProtocol::Messages)
        .and_then(|c| c.targets.first())
        .context("base target")?
        .clone();
    // The preferred native target now exists only under a different selector.
    for chain in &mut route.chains {
        if chain.selector == "fixture:model" {
            for target in &mut chain.targets {
                target.target["model"] = json!("different");
            }
        } else if chain.inbound == ApiProtocol::Messages {
            chain.targets.push(base_target.clone());
        }
    }
    let rejected = view(changed)?;
    ensure!(matches!(
        rejected.select_native("literal", ModelOperation::Generation),
        Err(ErrorCode::CapabilityMismatch)
    ));
    let wire = checked(rejected.select("literal", &ApiProtocol::Messages))?;
    assert_eq!(
        targets(&wire, "literal")
            .await?
            .first()
            .context("wire target")?
            .service_id,
        "different"
    );

    // A different immutable ref cannot be substituted for a matching projection.
    let mut changed = envelope(&input, None)?;
    let data = retained::decode(serde_json::from_value(serde_json::to_value(&changed)?)?)?;
    let data = data.routes.get("literal").context("typed route")?;
    let source = changed.artifacts.first_mut().context("artifact")?;
    let Definition::Route {
        routing,
        credentials,
        selector,
        ..
    } = &mut source.definition
    else {
        anyhow::bail!("expected route");
    };
    credentials
        .first_mut()
        .context("credential")?
        .credential
        .revision_id = "different-revision".try_into()?;
    let result = derive(
        selector,
        routing,
        credentials,
        &data.chains,
        &data.candidates,
        &mut Budget::new(),
    );
    ensure!(matches!(
        result,
        NativeChains::Unavailable(Unavailable::Unsupported)
    ));
    Ok(())
}

#[tokio::test]
async fn decisions_only_is_supported_without_generation_defaults_or_fabricated_generation_chain()
-> TestResult<()> {
    let mut input = basic("decide")?;
    if let Definition::Route {
        routing, selector, ..
    } = &mut input.definition
    {
        routing["providers"]["fixture"]["api_protocol"] = json!([{"*":"decisions"}]);
        routing["routers"] = json!({"fixed":{
            "selection":{"kind":"model","model":"fixture:model"},
            "defaults":{"system_prompt":"PRIVATE_DEFAULT","params":{"top_p":0.7}}
        }});
        *selector = "bitrouter/fixed".into();
    }
    let view = view(envelope(&input, None)?)?;
    ensure!(matches!(
        view.select_native("decide", ModelOperation::Generation),
        Err(ErrorCode::CapabilityMismatch)
    ));
    let route = checked(view.select_native("decide", ModelOperation::Decisions))?;
    let resolved = route.resolve_model("decide").await?;
    ensure!(resolved.prefs.inbound_protocol.is_none());
    assert_eq!(resolved.prefs.operation, ModelOperation::Decisions);
    ensure!(resolved.overrides.system_prompt.is_none() && resolved.overrides.params.is_empty());
    ensure!(
        route
            .preset_overrides("decide")
            .await?
            .system_prompt
            .is_none()
    );
    let prefs = RoutingPrefs {
        operation: ModelOperation::Decisions,
        ..Default::default()
    };
    let chain = route
        .route_chain("decide", &prefs, &CallerContext::local())
        .await?;
    assert_eq!(
        chain.first().context("decision target")?.api_protocol,
        ApiProtocol::Decisions
    );
    Ok(())
}

#[tokio::test]
async fn policy_tier_equal_to_public_resource_id_keeps_its_internal_native_chain() -> TestResult<()>
{
    let mut input = basic("fixture:cheap")?;
    let policy_ref = reference(ResourceKind::Policy, "policy")?;
    if let Definition::Route {
        routing,
        selector,
        policy,
        ..
    } = &mut input.definition
    {
        *selector = "bitrouter/choice".into();
        *policy = Some(policy_ref.clone());
        routing["routers"] = json!({"choice":{
            "selection":{"kind":"policy","policy":"selected","base_model":"fixture:base"}
        }});
    }
    let mut lock = crate::policy_lock::PolicyLock::default();
    lock.policies.insert(
        "selected".into(),
        crate::policy_lock::PolicyDefinition {
            tiers: [(
                "economy".into(),
                bitrouter_ai::routing::PolicyModelTarget::model_only("fixture:cheap".into()),
            )]
            .into(),
            default_tier: Some("economy".into()),
            ..Default::default()
        },
    );
    let artifact = Artifact {
        reference: policy_ref.clone(),
        envelope: None,
        definition: Definition::Policy {
            document: lock,
            name: "selected".into(),
            dependencies: Vec::new(),
        },
    };
    let view = view(envelope(&input, Some(artifact))?)?;
    let route = checked(view.select_native("fixture:cheap", ModelOperation::Generation))?;
    assert_eq!(
        route.resolve_model("fixture:cheap").await?.clean_model,
        "bitrouter/choice"
    );
    assert_eq!(
        targets(&route, "fixture:cheap")
            .await?
            .first()
            .context("base")?
            .service_id,
        "base"
    );
    let tier = route
        .route_resolved(
            "fixture:cheap",
            &RoutingPrefs::default(),
            &CallerContext::local(),
        )
        .await?;
    assert_eq!(tier.first().context("tier")?.service_id, "cheap");
    assert_eq!(route.policy().context("policy")?.reference(), &policy_ref);
    assert_eq!(route.policy().context("policy")?.name(), "selected");
    Ok(())
}
