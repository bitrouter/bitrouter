//! The `acp` protocol module — Agent Client Protocol.
//!
//! Spec refs:
//! - Protocol overview + schema: <https://agentclientprotocol.com/protocol/schema>
//! - Transport / stdio framing: <https://agentclientprotocol.com/protocol/transports>
//! - Initialization + capability negotiation:
//!   <https://agentclientprotocol.com/protocol/initialization>
//!
//! # One stack
//!
//! [`controller`] is the manager-facing, connection-level server: it owns one
//! harness connection and no session data, forwarding ACP verbatim in both
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
//! # What used to be here
//!
//! A second stack: `engine::Session`, a single conversation behind a
//! manager-facing id alias, driven by an ACP `Pipeline` of
//! `PreRequestHook` → `RouteHook` → `ExecutionHook` over a routing table
//! pinned to one target its executor ignored. Nothing outside this crate ever
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

pub mod transport;

#[cfg(feature = "acp")]
pub mod client;
#[cfg(feature = "acp")]
pub mod controller;
#[cfg(feature = "acp")]
pub mod telemetry;
#[cfg(feature = "acp")]
pub mod translate;
#[cfg(feature = "acp")]
pub mod up;
