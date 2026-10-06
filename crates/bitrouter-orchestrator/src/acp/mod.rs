//! The `acp` protocol module — Agent Client Protocol.
//!
//! Spec refs:
//! - Protocol overview + schema: <https://agentclientprotocol.com/protocol/schema>
//! - Transport / stdio framing: <https://agentclientprotocol.com/protocol/transports>
//! - Initialization + capability negotiation:
//!   <https://agentclientprotocol.com/protocol/initialization>
//!
//! # External controller and client
//!
//! [`controller`] is the manager-facing, connection-level server: it owns one
//! harness connection and delegates optional durable evidence capture to an
//! app-owned [`capture::CapturePort`], forwarding ACP verbatim in both
//! directions apart from the initialize gate, the endpoint plan it applies,
//! `_bitrouter/route/*`, and the attributed cost it decorates
//! `usage_update` with.
//!
//! [`client`] is the **one** BitRouter ACP client. It is transport-generic, so
//! the same type drives either a harness child directly or an in-process
//! [`controller::Controller`] over a duplex channel, on the caller's runtime.
//! `chat`, `chat_plain` and `acp prompt` are all consumers of it; they differ
//! in what they do with the update stream, not in how they speak ACP.
//!
//! [`up`] is the agent-process transport ([`up::AgentProcess`]) plus typed
//! initialize-only health checking ([`up::health_check`]). [`translate`] turns
//! raw `session/update` notifications into a typed enum — it is pure, and it
//! is the published wire contract of `acp prompt`'s NDJSON output.
//!
//! Transport configuration is carried by
//! `bitrouter_sdk::config::agent` alongside the shared product config, without
//! depending on the ACP SDK. This protocol stack belongs to the orchestrator;
//! the app supplies host configuration, route/cost services and durable capture.
//!
//! [`native`] is the inbound v1/v2 agent over the host's existing ThreadService.
//! It projects native history and controls without using the external controller
//! as an execution backend. The app's stdio bridge owns bytes only; its EOF does
//! not cancel daemon-owned native Turns or pending approval.
//!
//! # What used to be here
//!
//! A second stack: `engine::Session`, a single conversation behind a
//! manager-facing id alias, driven by an ACP `Pipeline` of
//! `PreRequestHook` → `RouteHook` → `ExecutionHook` over a routing table
//! pinned to one target its executor ignored. No consumer ever
//! registered a hook on it, and two stacks meant a harness could be offered
//! different client capabilities depending on which command launched it —
//! capabilities are declared by the client, so there has to be one.
//!
//! It is gone, along with `turn::TurnController`,
//! `permissions::PermissionRegistry`, `session::SessionState`,
//! `config_routing::ConfigAcpRoutingTable`, and the down-facing endpoint.
//! `docs/ACP_CONTROLLER_AMENDMENT_1.md` §2 records where each part went, and
//! `docs/ACP_SAFETY_INVARIANTS.md` records which of its guarantees moved and
//! what pins them now.

#![forbid(unsafe_code)]

pub mod capture;
pub mod client;
pub mod controller;
pub mod native;
pub mod telemetry;
pub mod translate;
pub mod up;
