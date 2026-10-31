//! Cargo-built broker tests using only public SDK provisioning and inference APIs.
use super::{
    CASE_TIMEOUT, CLEANUP_TIMEOUT, CONTROL_TOKEN, DaemonCommand, DaemonResponse, Host, STEP_TIMEOUT,
};
use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::{
    decisions::DecisionRequest,
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
    stream::{ProtocolEnd, StreamItem},
    tenancy::{
        execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState},
        identity::{AuthorityRevision, CorrelationId, RevisionId},
        operation::{
            DispatchBinding, DispatchState, ErrorCode, Mutation, OperationResult, OperationStatus,
        },
        resources::ResourceRevoke,
        usage_evidence::{
            Capture, Dimension, ExecutionUsageEvidenceV1, Protocol as EvidenceProtocol, Semantics,
            Source,
        },
        validation::Validate,
    },
};
use bitrouter_sdk::{
    gateway::{
        GatewayClient, HttpVersion, TransportOptions,
        tenancy::{Body, Error, Metadata, Problem, Reply},
    },
    tenancy::TenancyClient,
};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

#[path = "tenant_service/fixture.rs"]
mod fixture;
use fixture::{Owned, Protocol, SECRET_A, SECRET_B, SECRET_C};
const BODY_LIMIT: usize = 64 * 1024;

fn applied(result: &OperationResult) -> Result<AuthorityRevision> {
    result.validate()?;
    ensure!(result.status == OperationStatus::Applied);
    result
        .authority_revision
        .context("applied authority revision")
}

async fn receipt(
    owned: &Owned,
    correlation: &CorrelationId,
    metadata: &Metadata,
    protocol: Protocol,
    streaming: bool,
) -> Result<ExecutionReceipt> {
    ensure!(metadata.status == 200);
    let id = metadata
        .execution_id
        .as_ref()
        .context("admitted execution identity")?;
    let receipt = owned
        .reader
        .execution(&owned.installation, &owned.tenant, id)
        .await?;
    receipt.validate()?;
    ensure!(
        receipt.installation_id == owned.installation
            && receipt.tenant_id == owned.tenant
            && receipt.principal_id == owned.principal
    );
    ensure!(
        receipt.execution_id == *id
            && receipt.execution_id.as_str() != correlation.as_str()
            && receipt.correlation_id == *correlation
            && receipt.execution_revision == owned.revision.revision_id
    );
    ensure!(
        receipt.outcome == Outcome::Completed
            && receipt.attempts.len() == 1
            && receipt.attempts[0].outcome == Outcome::Completed
    );
    let target = receipt.attempts[0]
        .target
        .as_ref()
        .context("actual provider target")?;
    ensure!(
        target.provider_id.as_str() == "mock"
            && target.model_id == "model"
            && target.credential_version == owned.slot.revision_id
    );
    ensure!(
        target.account_id.as_str().starts_with("acct1-")
            && target.credential_id.as_str().starts_with("cred1-")
    );
    let messages = protocol == Protocol::Messages;
    ensure!(
        receipt.usage.input_tokens == Some(if messages { 16 } else { 11 })
            && receipt.usage.output_tokens == Some(7)
    );
    ensure!(
        receipt.usage.cache_read_tokens == Some(3) && receipt.usage.cache_write_tokens == Some(2)
    );
    ensure!(receipt.usage.reasoning_tokens == if messages { None } else { Some(4) });
    ensure!(receipt.usage.provenance == Provenance::Provider && receipt.usage.complete != messages);
    ensure!(
        receipt.state
            == if messages {
                ReceiptState::Incomplete
            } else {
                ReceiptState::Committed
            }
    );
    owned.public(&receipt)?;
    evidence(owned, &receipt, protocol, streaming, true).await?;
    Ok(receipt)
}

async fn evidence(
    owned: &Owned,
    receipt: &ExecutionReceipt,
    protocol: Protocol,
    streaming: bool,
    terminal: bool,
) -> Result<ExecutionUsageEvidenceV1> {
    let evidence = owned
        .reader
        .usage_evidence(&owned.installation, &owned.tenant, &receipt.execution_id)
        .await?;
    evidence.validate()?;
    ensure!(
        evidence.installation_id == receipt.installation_id
            && evidence.tenant_id == receipt.tenant_id
            && evidence.execution_id == receipt.execution_id
            && evidence.parent_execution_id.is_none()
            && evidence.child_execution_ids.is_empty()
    );
    ensure!(
        evidence.capture == Capture::Captured
            && !evidence.truncated
            && evidence.dropped_frames == 0
    );
    ensure!(
        evidence.attempts.len() == 1
            && evidence.attempts[0].attempt_id == receipt.attempts[0].attempt_id
    );
    ensure!(
        evidence.attempts[0].terminal_observed == terminal
            && evidence.attempts[0].outcome == receipt.attempts[0].outcome
    );
    ensure!(
        serde_json::to_value(&evidence.attempts[0].target)?
            == serde_json::to_value(&receipt.attempts[0].target)?
    );
    let expected_protocol = match protocol {
        Protocol::Chat => EvidenceProtocol::ChatCompletions,
        Protocol::Messages => EvidenceProtocol::Messages,
        Protocol::Responses => EvidenceProtocol::Responses,
        Protocol::Decisions => EvidenceProtocol::Decisions,
    };
    use Dimension::{
        CacheReadTokens, CacheWriteTokens, InputTokens, OutputTokens, ReasoningTokens, TotalTokens,
    };
    let full = vec![
        (InputTokens, 11),
        (OutputTokens, 7),
        (TotalTokens, 18),
        (CacheReadTokens, 3),
        (ReasoningTokens, 4),
    ];
    let partial = vec![
        (InputTokens, 5),
        (OutputTokens, 1),
        (TotalTokens, 6),
        (CacheReadTokens, 1),
        (ReasoningTokens, 0),
    ];
    // This is the native contract, including explicit zero, absent fields and
    // repeated snapshots. In particular, Messages input stays 11, not normalized 16.
    let expected = match (protocol, streaming, terminal) {
        (Protocol::Chat, true, false) => vec![(Source::ChatChunk, partial)],
        (Protocol::Chat, true, true) => vec![
            (Source::ChatChunk, partial),
            (Source::ChatChunk, full.clone()),
            (Source::ChatChunk, full),
        ],
        (Protocol::Messages, true, true) => vec![
            (
                Source::MessageStart,
                vec![
                    (InputTokens, 11),
                    (OutputTokens, 0),
                    (CacheReadTokens, 3),
                    (CacheWriteTokens, 2),
                ],
            ),
            (Source::MessageDelta, vec![(OutputTokens, 7)]),
        ],
        (Protocol::Messages, false, true) => vec![(
            Source::Json,
            vec![
                (InputTokens, 11),
                (OutputTokens, 7),
                (CacheReadTokens, 3),
                (CacheWriteTokens, 2),
            ],
        )],
        (Protocol::Responses, true, true) => vec![(Source::ResponseTerminal, full)],
        (Protocol::Chat | Protocol::Responses, false, true) => vec![(Source::Json, full)],
        (Protocol::Decisions, false, true) => vec![(
            Source::Json,
            vec![(InputTokens, 11), (OutputTokens, 7), (TotalTokens, 18)],
        )],
        _ => bail!("unsupported evidence fixture"),
    };
    ensure!(
        evidence.frames.len() == expected.len(),
        "native frame count changed"
    );
    for (index, (frame, (source, quantities))) in evidence.frames.iter().zip(expected).enumerate() {
        ensure!(
            frame.sequence == index as u64
                && frame.protocol == expected_protocol
                && frame.attempt_id == receipt.attempts[0].attempt_id
                && frame.source == source
                && frame.semantics
                    == if protocol == Protocol::Decisions {
                        Semantics::Unknown
                    } else {
                        Semantics::Snapshot
                    }
                && !frame.invalid
                && frame.unsupported == (protocol != Protocol::Messages)
        );
        let actual: Vec<_> = frame
            .quantities
            .iter()
            .map(|q| (q.dimension, q.value))
            .collect();
        let expected: Vec<_> = quantities
            .into_iter()
            .map(|(dimension, value)| (dimension, Some(value)))
            .collect();
        ensure!(
            actual == expected,
            "native quantities changed in frame {index}"
        );
    }
    owned.public(&evidence)?;
    Ok(evidence)
}

async fn consume<T: Serialize, E: Serialize>(
    owned: &Owned,
    correlation: &CorrelationId,
    protocol: Protocol,
    streaming: bool,
    reply: Reply<Body<T, E>>,
) -> Result<ExecutionReceipt> {
    match reply.body {
        Body::Json(body) => {
            ensure!(!streaming);
            let value = serde_json::to_value(body)?;
            let text = match protocol {
                Protocol::Chat => &value["choices"][0]["message"]["content"],
                Protocol::Messages => &value["content"][0]["text"],
                Protocol::Responses => &value["output"][0]["content"][0]["text"],
                Protocol::Decisions => bail!("unexpected Decisions relay"),
            };
            ensure!(
                text == if protocol == Protocol::Chat {
                    owned.reply
                } else {
                    fixture::REPLY
                }
            );
            owned.public(&value)?;
            receipt(owned, correlation, &reply.metadata, protocol, false).await
        }
        Body::Stream(mut stream) => {
            ensure!(streaming);
            let mut text = String::new();
            let mut events = 0;
            let mut terminal = None;
            while let Some(item) = stream.next().await {
                ensure!(terminal.is_none(), "event after protocol terminal");
                match item? {
                    StreamItem::Event(event) => {
                        events += 1;
                        ensure!(events <= 16);
                        let value = serde_json::to_value(event.data)?;
                        owned.public(&value)?;
                        let delta = match protocol {
                            Protocol::Chat => &value["choices"][0]["delta"]["content"],
                            Protocol::Messages => &value["delta"]["text"],
                            Protocol::Responses => &value["delta"],
                            Protocol::Decisions => bail!("unexpected Decisions stream"),
                        };
                        if let Some(delta) = delta.as_str() {
                            text.push_str(delta);
                        }
                    }
                    StreamItem::End(end) => {
                        ensure!(end == protocol.end()? && text == fixture::REPLY && events > 0);
                        // Query immediately at the explicit terminal, without polling or waiting for EOF.
                        terminal = Some(
                            receipt(owned, correlation, &reply.metadata, protocol, true).await?,
                        );
                    }
                }
            }
            terminal.context("stream ended without protocol terminal")
        }
    }
}

async fn dispatch(
    client: &GatewayClient,
    owned: &Owned,
    protocol: Protocol,
    stream: bool,
    correlation: &CorrelationId,
) -> Result<ExecutionReceipt> {
    let binding = owned.binding(correlation).await?;
    let scoped = client.tenant(owned.token()?, &binding)?;
    let request = protocol.request(stream);
    match protocol {
        Protocol::Chat => {
            consume(
                owned,
                correlation,
                protocol,
                stream,
                scoped
                    .chat_reply(&serde_json::from_value::<ChatRequest>(request)?)
                    .await?,
            )
            .await
        }
        Protocol::Messages => {
            consume(
                owned,
                correlation,
                protocol,
                stream,
                scoped
                    .messages_reply(&serde_json::from_value::<MessagesRequest>(request)?)
                    .await?,
            )
            .await
        }
        Protocol::Responses => {
            consume(
                owned,
                correlation,
                protocol,
                stream,
                scoped
                    .responses_reply(&serde_json::from_value::<ResponsesRequest>(request)?)
                    .await?,
            )
            .await
        }
        Protocol::Decisions => {
            ensure!(!stream);
            let reply = scoped
                .decisions(&serde_json::from_value::<DecisionRequest>(request)?)
                .await?;
            let value = serde_json::to_value(reply.body)?;
            ensure!(
                value["answers"][0]["name"] == "q" && value["answers"][0]["probability"] == 0.7
            );
            owned.public(&value)?;
            receipt(owned, correlation, &reply.metadata, protocol, false).await
        }
    }
}

async fn protocol_matrix(version: HttpVersion) -> Result<()> {
    let provider = MockServer::start().await;
    let mut gate = fixture::ProviderGate::start(&provider.uri()).await?;
    let protocols = [
        Protocol::Chat,
        Protocol::Messages,
        Protocol::Responses,
        Protocol::Decisions,
    ];
    for protocol in protocols {
        fixture::mount(&provider, protocol, SECRET_A, false).await?;
        if protocol != Protocol::Decisions {
            fixture::mount(&provider, protocol, SECRET_A, true).await?;
        }
    }
    fixture::mount(&provider, Protocol::Chat, SECRET_B, false).await?;
    let (mut host, key) = fixture::start(&gate.endpoint)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        host.ready().await?;
        let owned = fixture::provision(
            &host,
            &gate.endpoint,
            "protocol-tenant",
            SECRET_A,
            &protocols,
            version,
        )
        .await?;
        let beta = fixture::provision(
            &host,
            &gate.endpoint,
            "beta",
            SECRET_B,
            &[Protocol::Chat],
            version,
        )
        .await?;
        let default = fixture::default_owner(&host, &gate.endpoint, version).await?;
        let client = fixture::gateway(&host, default.token()?, version)?;
        client.health().await?;
        concurrent_tenants(&mut gate, &client, &owned, &beta).await?;
        let correlation: CorrelationId = "seven-protocol-modes".try_into()?;
        let mut identities = std::collections::HashSet::new();
        let mut sends = vec![
            (Protocol::Chat, SECRET_A, false),
            (Protocol::Chat, SECRET_B, false),
            (Protocol::Chat, SECRET_A, false),
        ];
        for protocol in protocols {
            let streams: &[bool] = if protocol == Protocol::Decisions {
                &[false]
            } else {
                &[false, true]
            };
            for &stream in streams {
                let receipt = dispatch(&client, &owned, protocol, stream, &correlation).await?;
                ensure!(identities.insert(receipt.execution_id));
                sends.push((protocol, SECRET_A, stream));
                fixture::calls(&provider, &sends).await?;
            }
        }
        ensure!(identities.len() == 7);
        let page = owned
            .reader
            .executions(&owned.installation, &owned.tenant, &correlation, None, 10)
            .await?;
        ensure!(
            page.multiple_executions && page.next_after.is_none() && page.execution_ids.len() == 7
        );
        ensure!(page.execution_ids.iter().all(|id| identities.contains(id)));
        fixture::stop(&mut host).await?;
        fixture::calls(&provider, &sends).await?;
        fixture::clean(
            &host.logs(),
            &[owned.token()?, beta.token()?, default.token()?, &key],
        )
    })
    .await
    .context("dynamic protocol matrix timeout")
    .and_then(|result| result);
    let cleanup = gate.close().await;
    host.finish(result.and(cleanup))
}

async fn concurrent_tenants(
    gate: &mut fixture::ProviderGate,
    gateway: &GatewayClient,
    alpha: &Owned,
    beta: &Owned,
) -> Result<(ExecutionReceipt, ExecutionReceipt)> {
    let clone = gateway.clone();
    let correlation: CorrelationId = "same-correlation-two-real-executions".try_into()?;
    let beta_correlation = "beta-isolated".try_into()?;
    gate.arm()?;
    // The provider cannot respond to either request until it observes both
    // credentials. This fails a serialized implementation even on a warm pool.
    let (first, beta_receipt, ()) = tokio::try_join!(
        dispatch(gateway, alpha, Protocol::Chat, false, &correlation),
        dispatch(&clone, beta, Protocol::Chat, false, &beta_correlation),
        gate.release_pair(),
    )?;
    let second = dispatch(&clone, alpha, Protocol::Chat, false, &correlation).await?;
    ensure!(
        first.execution_id != second.execution_id
            && first.principal_id != beta_receipt.principal_id
    );
    let page = alpha
        .reader
        .executions(&alpha.installation, &alpha.tenant, &correlation, None, 10)
        .await?;
    ensure!(page.multiple_executions && page.next_after.is_none() && page.execution_ids.len() == 2);
    ensure!(
        page.execution_ids.contains(&first.execution_id)
            && page.execution_ids.contains(&second.execution_id)
    );
    Ok((first, beta_receipt))
}

#[tokio::test]
async fn sdk_dynamic_http1_all_seven_protocol_modes() -> Result<()> {
    protocol_matrix(HttpVersion::Http1Only).await
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn sdk_dynamic_h2c_all_seven_protocol_modes() -> Result<()> {
    // Prior knowledge requires h2c on both real broker listeners; the SDK never falls back.
    protocol_matrix(HttpVersion::Http2PriorKnowledge).await
}

async fn rejected(
    client: &GatewayClient,
    owned: &Owned,
    binding: &DispatchBinding,
    code: ErrorCode,
) -> Result<()> {
    let result = client
        .tenant(owned.token()?, binding)?
        .chat_reply(&serde_json::from_value(Protocol::Chat.request(false))?)
        .await;
    let error = result
        .err()
        .context("rejected call unexpectedly succeeded")?;
    ensure!(error.dispatch_state() == DispatchState::NotStarted);
    let Error::Response(failure) = error else {
        bail!("expected structured service rejection");
    };
    ensure!(failure.problem.is_none() && failure.metadata.execution_id.is_none());
    let body = failure.tenancy_error.context("typed tenancy rejection")?;
    ensure!(body.code == code && body.execution_id.is_none());
    let page = owned
        .operator
        .executions(
            &owned.installation,
            &owned.tenant,
            &binding.correlation_id,
            None,
            10,
        )
        .await?;
    ensure!(page.execution_ids.is_empty());
    Ok(())
}

async fn revoke_resource(
    owned: &Owned,
    reference: &bitrouter_ai::tenancy::configuration::ResourceRef,
    operation: &str,
) -> Result<()> {
    let status = owned
        .reader
        .resource_status(&owned.installation, &owned.tenant, reference)
        .await?;
    applied(
        &owned
            .reader
            .revoke_resource(
                &owned.tenant,
                &ResourceRevoke {
                    format: 1,
                    operation_id: operation.try_into()?,
                    installation_id: owned.installation.clone(),
                    reference: reference.clone(),
                    expected_authority_revision: status.authority_revision,
                },
            )
            .await?,
    )?;
    Ok(())
}

#[tokio::test]
async fn sdk_pool_tenant_isolation_rotation_restart_and_revocation() -> Result<()> {
    let provider = MockServer::start().await;
    let mut gate = fixture::ProviderGate::start(&provider.uri()).await?;
    for secret in [SECRET_A, SECRET_B, SECRET_C] {
        fixture::mount(&provider, Protocol::Chat, secret, false).await?;
    }
    let (mut host, key) = fixture::start(&gate.endpoint)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        host.ready().await?;
        let mut alpha = fixture::provision(
            &host,
            &gate.endpoint,
            "alpha",
            SECRET_A,
            &[Protocol::Chat],
            HttpVersion::Http1Only,
        )
        .await?;
        let beta = fixture::provision(
            &host,
            &gate.endpoint,
            "beta",
            SECRET_B,
            &[Protocol::Chat],
            HttpVersion::Http1Only,
        )
        .await?;
        let default = fixture::default_owner(&host, &gate.endpoint, HttpVersion::Http1Only).await?;
        let gateway = fixture::gateway(&host, default.token()?, HttpVersion::Http1Only)?;
        gateway.health().await?;
        let clone = gateway.clone();
        let (first, beta_receipt) = concurrent_tenants(&mut gate, &gateway, &alpha, &beta).await?;
        let completed_evidence = evidence(&alpha, &first, Protocol::Chat, false, true).await?;
        // No binding was installed on the shared client by tenant calls.
        ensure!(
            gateway
                .chat_completions(&serde_json::from_value(Protocol::Chat.request(false))?)
                .await
                .is_err()
        );
        clone.health().await?;
        let mut expected = vec![
            (Protocol::Chat, SECRET_A, false),
            (Protocol::Chat, SECRET_B, false),
            (Protocol::Chat, SECRET_A, false),
        ];
        fixture::calls(&provider, &expected).await?;
        let mut stale_capability = alpha.binding(&"stale-capability".try_into()?).await?;
        stale_capability.capability_revision = "obsolete-capability".try_into()?;
        rejected(
            &clone,
            &alpha,
            &stale_capability,
            ErrorCode::CapabilityMismatch,
        )
        .await?;
        fixture::calls(&provider, &expected).await?;
        let old = alpha.rotate(&gate.endpoint).await?;
        ensure!(old.revision_id != alpha.slot.revision_id);
        dispatch(
            &gateway,
            &alpha,
            Protocol::Chat,
            false,
            &"after-rotation".try_into()?,
        )
        .await?;
        expected.push((Protocol::Chat, SECRET_C, false));
        // Beta's active configuration still references this warmed credential.
        // Revoking it must fence the cached route, without rotating anything away.
        dispatch(
            &clone,
            &beta,
            Protocol::Chat,
            false,
            &"warm-active-upstream".try_into()?,
        )
        .await?;
        expected.push((Protocol::Chat, SECRET_B, false));
        revoke_resource(&beta, &beta.slot, "revoke-active-upstream").await?;
        rejected(
            &clone,
            &beta,
            &beta
                .binding(&"revoked-upstream-before-restart".try_into()?)
                .await?,
            ErrorCode::Forbidden,
        )
        .await?;
        fixture::calls(&provider, &expected).await?;
        revoke_resource(&alpha, &old, "revoke-old-upstream").await?;
        dispatch(
            &clone,
            &alpha,
            Protocol::Chat,
            false,
            &"old-revocation-is-exact".try_into()?,
        )
        .await?;
        expected.push((Protocol::Chat, SECRET_C, false));
        let before = alpha.reader.capabilities().await?;
        let stale = before.bind(
            alpha.revision.revision_id.clone(),
            "stale-instance".try_into()?,
        )?;
        fixture::stop(&mut host).await?;
        host.spawn_with_wrapping_key(&host.home.join("private.yaml"), Some(&key))?;
        host.ready().await?;
        let after = alpha.reader.capabilities().await?;
        ensure!(
            after.installation_id == before.installation_id
                && after.instance_id != before.instance_id
        );
        let old_status = alpha
            .reader
            .resource_status(&alpha.installation, &alpha.tenant, &old)
            .await?;
        ensure!(
            old_status.revoked
                && old_status.head_revision.as_ref() == Some(&alpha.slot.revision_id)
        );
        alpha.public(&old_status)?;
        ensure!(
            !alpha
                .reader
                .resource_status(&alpha.installation, &alpha.tenant, &alpha.slot)
                .await?
                .revoked
        );
        ensure!(
            alpha
                .reader
                .active_revision(&alpha.installation, &alpha.tenant)
                .await?
                .revision_id
                .as_ref()
                == Some(&alpha.revision.revision_id)
        );
        let retained = alpha
            .reader
            .execution(&alpha.installation, &alpha.tenant, &first.execution_id)
            .await?;
        ensure!(serde_json::to_value(&retained)? == serde_json::to_value(&first)?);
        let retained_evidence = evidence(&alpha, &retained, Protocol::Chat, false, true).await?;
        ensure!(
            serde_json::to_value(&retained_evidence)? == serde_json::to_value(&completed_evidence)?
        );
        let status = beta
            .reader
            .resource_status(&beta.installation, &beta.tenant, &beta.slot)
            .await?;
        ensure!(status.revoked && status.head_revision.as_ref() == Some(&beta.slot.revision_id));
        ensure!(
            beta.reader
                .active_revision(&beta.installation, &beta.tenant)
                .await?
                .revision_id
                .as_ref()
                == Some(&beta.revision.revision_id)
        );
        rejected(
            &clone,
            &beta,
            &beta
                .binding(&"revoked-upstream-after-restart".try_into()?)
                .await?,
            ErrorCode::Forbidden,
        )
        .await?;
        rejected(&clone, &alpha, &stale, ErrorCode::CapabilityMismatch).await?;
        fixture::calls(&provider, &expected).await?;
        dispatch(
            &gateway,
            &alpha,
            Protocol::Chat,
            false,
            &"restart-current-identity".try_into()?,
        )
        .await?;
        expected.push((Protocol::Chat, SECRET_C, false));
        let route = alpha.routes.first().context("current route")?;
        revoke_resource(&alpha, route, "revoke-current-route").await?;
        let status = alpha
            .reader
            .resource_status(&alpha.installation, &alpha.tenant, route)
            .await?;
        ensure!(status.revoked && status.head_revision.as_ref() == Some(&route.revision_id));
        alpha.public(&status)?;
        rejected(
            &gateway,
            &alpha,
            &alpha.binding(&"revoked-route".try_into()?).await?,
            ErrorCode::Forbidden,
        )
        .await?;
        fixture::calls(&provider, &expected).await?;
        let current = beta
            .operator
            .credential(&beta.installation, &beta.tenant, &beta.issued.credential_id)
            .await?;
        applied(
            &beta
                .operator
                .revoke_credential(
                    &beta.tenant,
                    &beta.issued.credential_id,
                    &Mutation {
                        operation_id: "revoke-beta-ingress".try_into()?,
                        installation_id: beta.installation.clone(),
                        expected_revision: current.credential_revision,
                    },
                )
                .await?,
        )?;
        let fresh = beta.operator.capabilities().await?.bind(
            beta.revision.revision_id.clone(),
            "revoked-ingress".try_into()?,
        )?;
        rejected(&clone, &beta, &fresh, ErrorCode::Unauthorized).await?;
        fixture::calls(&provider, &expected).await?;
        ensure!(
            serde_json::to_value(
                beta.operator
                    .execution(&beta.installation, &beta.tenant, &beta_receipt.execution_id)
                    .await?
            )? == serde_json::to_value(&beta_receipt)?
        );
        fixture::stop(&mut host).await?;
        fixture::calls(&provider, &expected).await?;
        fixture::clean(
            &host.logs(),
            &[alpha.token()?, beta.token()?, default.token()?, &key],
        )
    })
    .await
    .context("dynamic pool lifecycle timeout")
    .and_then(|result| result);
    let cleanup = gate.close().await;
    host.finish(result.and(cleanup))
}

async fn abandoned_stream(timeout: bool) -> Result<()> {
    let mut provider = fixture::HeldStream::start().await?;
    let (mut host, key) = fixture::start(&provider.endpoint)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        host.ready().await?;
        let owned = fixture::provision(
            &host,
            &provider.endpoint,
            "abandoned",
            SECRET_A,
            &[Protocol::Chat],
            HttpVersion::Http1Only,
        )
        .await?;
        let gateway = GatewayClient::with_options(
            &format!("http://{}", host.inference),
            None,
            TransportOptions {
                http_version: HttpVersion::Http1Only,
                connect_timeout: Duration::from_secs(1),
                read_timeout: Some(Duration::from_secs(1)),
                request_timeout: Some(STEP_TIMEOUT),
                max_event_bytes: BODY_LIMIT,
                max_response_bytes: BODY_LIMIT,
                ..Default::default()
            },
        )?;
        let correlation: CorrelationId = "abandoned-no-replay".try_into()?;
        let binding = owned.binding(&correlation).await?;
        let started = tokio::time::Instant::now();
        let reply = gateway
            .tenant(owned.token()?, &binding)?
            .chat_reply(&serde_json::from_value(Protocol::Chat.request(true))?)
            .await?;
        let id = reply
            .metadata
            .execution_id
            .context("candidate identity before stream failure")?;
        let Body::Stream(mut stream) = reply.body else {
            bail!("expected held SSE");
        };
        ensure!(matches!(
            stream.next().await.context("initial provider event")??,
            StreamItem::Event(_)
        ));
        let (authorized, body) = tokio::time::timeout(STEP_TIMEOUT, provider.observed.recv())
            .await?
            .context("held provider observation")?;
        ensure!(authorized && body["model"] == "model" && body["stream"] == true);
        if timeout {
            // The SDK's one-second read deadline must fire before the broker's
            // three-second idle timer. A server terminal/EOF cannot satisfy this.
            let deadline = started + Duration::from_secs(2);
            let error = tokio::time::timeout_at(deadline, stream.next())
                .await
                .context("SDK read timeout did not precede server idle timeout")?
                .context("timeout stream ended silently")?
                .err()
                .context("held stream unexpectedly completed")?;
            ensure!(tokio::time::Instant::now() < deadline);
            ensure!(error.dispatch_state() == DispatchState::Unknown);
            let Error::Response(failure) = error else {
                bail!("missing stream failure metadata");
            };
            ensure!(
                failure.metadata.execution_id.as_ref() == Some(&id)
                    && failure.problem == Some(Problem::Transport)
                    && failure.tenancy_error.is_none()
                    && failure.gateway_error.is_none()
            );
        }
        drop(stream);
        // Cancellation is asynchronous. Poll only public evidence, never resend inference.
        let unknown = tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                match owned
                    .reader
                    .execution(&owned.installation, &owned.tenant, &id)
                    .await
                {
                    Ok(receipt) => break Ok::<_, anyhow::Error>(receipt),
                    Err(bitrouter_sdk::error::Error::Tenancy(failure))
                        if failure
                            .body
                            .as_ref()
                            .is_some_and(|body| body.code == ErrorCode::NotFound) =>
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await
        .context("unknown receipt was not retained")??;
        unknown.validate()?;
        ensure!(
            unknown.execution_id == id
                && unknown.correlation_id == correlation
                && unknown.tenant_id == owned.tenant
                && unknown.principal_id == owned.principal
        );
        ensure!(
            unknown.state == ReceiptState::Unknown
                && unknown.outcome != Outcome::Completed
                && unknown.attempts.len() == 1
                && unknown.attempts[0].outcome == Outcome::Unknown
        );
        ensure!(
            unknown.usage.input_tokens == Some(5)
                && unknown.usage.output_tokens == Some(1)
                && !unknown.usage.complete
        );
        owned.public(&unknown)?;
        let abandoned_evidence = evidence(&owned, &unknown, Protocol::Chat, true, false).await?;
        let page = owned
            .reader
            .executions(&owned.installation, &owned.tenant, &correlation, None, 10)
            .await?;
        ensure!(page.execution_ids == vec![id.clone()] && !page.multiple_executions);
        ensure!(provider.calls.load(Ordering::SeqCst) == 1);
        fixture::stop(&mut host).await?;
        ensure!(provider.calls.load(Ordering::SeqCst) == 1);
        host.spawn_with_wrapping_key(&host.home.join("private.yaml"), Some(&key))?;
        host.ready().await?;
        let retained = owned
            .reader
            .execution(&owned.installation, &owned.tenant, &id)
            .await?;
        ensure!(serde_json::to_value(&retained)? == serde_json::to_value(&unknown)?);
        let retained_evidence = evidence(&owned, &retained, Protocol::Chat, true, false).await?;
        ensure!(
            serde_json::to_value(&retained_evidence)? == serde_json::to_value(&abandoned_evidence)?
        );
        let retained_page = owned
            .reader
            .executions(&owned.installation, &owned.tenant, &correlation, None, 10)
            .await?;
        ensure!(
            retained_page.execution_ids == page.execution_ids && !retained_page.multiple_executions
        );
        ensure!(provider.calls.load(Ordering::SeqCst) == 1);
        fixture::stop(&mut host).await?;
        ensure!(provider.calls.load(Ordering::SeqCst) == 1);
        fixture::clean(&host.logs(), &[owned.token()?, &key])
    })
    .await
    .context("abandoned SDK stream timeout")
    .and_then(|result| result);
    let cleanup = provider.close().await.and_then(|()| {
        ensure!(
            provider.calls.load(Ordering::SeqCst) == 1,
            "provider replay during shutdown"
        );
        Ok(())
    });
    host.finish(result.and(cleanup))
}

#[tokio::test]
async fn sdk_dropped_sse_retains_unknown_receipt_without_replay() -> Result<()> {
    abandoned_stream(false).await
}

#[tokio::test]
async fn sdk_sse_read_timeout_retains_identity_and_unknown_receipt_without_replay() -> Result<()> {
    abandoned_stream(true).await
}
