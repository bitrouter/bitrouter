//! Authentication of retained compiler output; never resolves mutable inputs.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
        validation::Validate,
    },
    types::ApiProtocol,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    COMPILER_VERSION, Envelope, MAX_ARTIFACT_BYTES, MAX_CLOSURE, Resources,
    compiler::{self, RouteEnvelope},
    manifest::{ByteBudget, Definition, Import},
};
use crate::{runtime_crypto::SealedSecret, tenancy::configuration_store::StoredConfiguration};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Payload {
    Empty { binding: Value },
    Sealed { sealed: SealedSecret },
}

pub(super) fn compiled(
    resources: &Resources,
    stored: &StoredConfiguration,
) -> Result<super::execution::ValidatedEnvelope> {
    let revision = &stored.revision;
    ByteBudget::new(MAX_ARTIFACT_BYTES).charge(&revision.document)?;
    revision.validate()?;
    let mut canonical_document = serde_json::to_value(&revision.document)?;
    crate::tenancy::command::sort(&mut canonical_document);
    let digest = hex::encode(Sha256::digest(super::manifest::bounded_bytes(
        &canonical_document,
        MAX_ARTIFACT_BYTES,
    )?));
    ensure!(
        revision.content_digest.as_str() == digest,
        "public configuration digest mismatch"
    );
    ensure!(
        revision.compiler_version == COMPILER_VERSION,
        "unsupported configuration compiler"
    );
    ensure!(
        stored.dependencies.len() <= MAX_CLOSURE,
        "configuration dependency capacity exceeded"
    );
    let dependencies: HashSet<_> = stored.dependencies.iter().collect();
    ensure!(
        dependencies.len() == stored.dependencies.len(),
        "duplicate configuration dependency"
    );
    ensure!(
        dependencies
            .iter()
            .all(|r| r.owner_kind != OwnerKind::Tenant
                || r.owner_id.as_str() == revision.tenant_id.as_str()),
        "foreign configuration dependency"
    );
    let document = &revision.document;
    let roots: Vec<_> = document
        .routes
        .iter()
        .chain(document.policy.iter())
        .collect();
    ensure!(
        roots.iter().all(|r| dependencies.contains(r)),
        "missing configuration root"
    );
    let mut names = HashSet::new();
    ensure!(
        document.routes.iter().all(|r| names.insert(&r.resource_id)),
        "duplicate visible route"
    );
    let binding = (
        &resources.installation,
        &revision.tenant_id,
        &revision.revision_id,
        COMPILER_VERSION,
        document,
        &stored.dependencies,
    );
    ByteBudget::new(MAX_ARTIFACT_BYTES - 256).charge(&binding)?;
    // Ciphertext/base64 expansion is bounded before cloning the JSON value.
    ByteBudget::new(2 * MAX_ARTIFACT_BYTES).charge(&stored.compiled)?;
    let envelope = match serde_json::from_value::<Payload>(stored.compiled.clone())? {
        Payload::Empty { binding: retained } => {
            ensure!(
                roots.is_empty() && dependencies.is_empty(),
                "nonempty empty configuration"
            );
            ensure!(
                retained == serde_json::to_value(binding)?,
                "empty configuration binding mismatch"
            );
            Envelope {
                document: document.clone(),
                routes: Vec::new(),
                artifacts: Vec::new(),
            }
        }
        Payload::Sealed { sealed } => {
            ensure!(!roots.is_empty(), "sealed empty configuration");
            let key = resources
                .key
                .as_ref()
                .context("configuration key unavailable")?;
            let bytes = key.open_bound("configuration", &binding, &sealed)?;
            let envelope: Envelope = serde_json::from_slice(&bytes)?;
            ensure!(envelope.document == *document, "compiled document mismatch");
            ensure!(
                envelope.artifacts.len() <= MAX_CLOSURE
                    && envelope.routes.len() == document.routes.len(),
                "compiled envelope count mismatch"
            );
            let mut artifacts = HashMap::new();
            for artifact in &envelope.artifacts {
                ensure!(
                    dependencies.contains(&artifact.reference)
                        && artifacts.insert(&artifact.reference, artifact).is_none(),
                    "unexpected compiled artifact"
                );
                Import {
                    reference: artifact.reference.clone(),
                    definition: artifact.definition.clone(),
                }
                .validate()?;
                ensure!(
                    artifact
                        .definition
                        .dependencies()
                        .iter()
                        .all(|r| dependencies.contains(r)),
                    "unexpected artifact dependency"
                );
                match (&artifact.definition, &artifact.envelope) {
                    (
                        Definition::Route {
                            selector, policy, ..
                        },
                        Some(route),
                    ) => {
                        ensure!(
                            route.reference == artifact.reference
                                && route.selector == *selector
                                && route.policy == *policy,
                            "retained route binding mismatch"
                        );
                        route_shape(route, &dependencies)?;
                    }
                    (Definition::Policy { .. }, None) => {}
                    _ => anyhow::bail!("invalid compiled artifact kind or shape"),
                }
            }
            // Exact reachable closure: credential slots are refs only; all other
            // dependencies must have one retained artifact. Orphans cannot hide
            // an extra resource grant or a secret-bearing credential definition.
            let mut queue = roots;
            let mut reached = HashSet::new();
            while let Some(reference) = queue.pop() {
                if !reached.insert(reference) {
                    continue;
                }
                if reference.resource_kind != ResourceKind::Credential {
                    let artifact = artifacts
                        .get(reference)
                        .context("missing compiled artifact")?;
                    for dependency in artifact.definition.dependencies() {
                        let retained = dependencies
                            .get(&dependency)
                            .context("missing compiled dependency")?;
                        if !reached.contains(retained) && !queue.contains(retained) {
                            queue.push(retained);
                        }
                    }
                }
            }
            ensure!(
                reached == dependencies
                    && artifacts.len()
                        == reached
                            .iter()
                            .filter(|r| r.resource_kind != ResourceKind::Credential)
                            .count(),
                "compiled closure mismatch"
            );
            for route in &envelope.routes {
                route_shape(route, &dependencies)?;
                if let (Some(reference), Some(name)) = (&route.policy, &route.policy_name) {
                    let source = artifacts
                        .get(&route.reference)
                        .context("retained route source missing")?;
                    let Definition::Route { routing, .. } = &source.definition else {
                        anyhow::bail!("retained route source kind")
                    };
                    let policy = artifacts
                        .get(reference)
                        .context("retained policy missing")?;
                    let Definition::Policy { document, .. } = &policy.definition else {
                        anyhow::bail!("retained policy kind")
                    };
                    let (config, _) = compiler::routing_config(routing)?;
                    crate::tenancy::trajectory_profile::retained_guarded(&config, document, name)?;
                }
            }
            let expected = compiler::configuration_routes(document, &envelope.artifacts)
                .map_err(|_| anyhow::anyhow!("invalid retained route composition"))?;
            ensure!(
                envelope.routes == expected,
                "compiled route composition mismatch"
            );
            envelope
        }
    };
    super::execution::validate(envelope)
}

fn route_shape(route: &RouteEnvelope, dependencies: &HashSet<&ResourceRef>) -> Result<()> {
    ByteBudget::new(MAX_ARTIFACT_BYTES).charge(route)?;
    ensure!(
        route.reference.resource_kind == ResourceKind::Route
            && dependencies.contains(&route.reference)
            && !route.selector.is_empty()
            && route.selector.len() <= 512
            && !route.chains.is_empty()
            && route.chains.len() <= 512 * 4,
        "invalid retained route envelope"
    );
    ensure!(
        match (&route.policy, &route.policy_name) {
            (None, None) => true,
            (Some(reference), Some(name)) =>
                reference.resource_kind == ResourceKind::Policy
                    && dependencies.contains(reference)
                    && !name.is_empty()
                    && name.len() <= 256,
            _ => false,
        },
        "invalid retained policy binding"
    );
    let mut chains = HashSet::new();
    for chain in &route.chains {
        ensure!(
            !chain.selector.is_empty()
                && chain.selector.len() <= 512
                && !matches!(chain.inbound, ApiProtocol::Custom(_))
                && chains.insert((&chain.selector, chain.inbound.as_str()))
                && !chain.targets.is_empty()
                && chain.targets.len() <= 256,
            "invalid retained route chain"
        );
        for target in &chain.targets {
            ensure!(
                target.credential.resource_kind == ResourceKind::Credential
                    && dependencies.contains(&target.credential),
                "unexpected route credential"
            );
            let fields = target
                .target
                .as_object()
                .context("invalid retained target")?;
            let names = [
                "provider",
                "model",
                "api_base",
                "protocol",
                "account",
                "auth_scheme",
                "chat_token_limit_field",
                "chat_supports_store",
                "chat_supports_stream_options",
                "chat_google_extensions",
                "reasoning_effort",
                "model_constraints",
                "headers",
            ];
            ensure!(
                fields.len() == names.len() && names.iter().all(|name| fields.contains_key(*name)),
                "invalid retained target fields"
            );
            ensure!(
                ["provider", "model", "api_base"]
                    .iter()
                    .all(|name| fields[*name].as_str().is_some_and(|s| !s.is_empty())),
                "invalid retained target identity"
            );
            crate::config::url_validator::validate_upstream_url(
                fields["api_base"]
                    .as_str()
                    .context("invalid retained target URL")?,
            )
            .map_err(|_| anyhow::anyhow!("invalid retained target URL"))?;
            let protocol: ApiProtocol = serde_json::from_value(fields["protocol"].clone())?;
            ensure!(
                !matches!(protocol, ApiProtocol::Custom(_))
                    && protocol.operation() == chain.inbound.operation(),
                "unqualified retained protocol"
            );
            ensure!(
                fields["headers"].is_array(),
                "invalid retained target headers"
            );
        }
    }
    Ok(())
}
