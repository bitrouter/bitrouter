# bitrouter-p2p

P2P networking layer for BitRouter using [iroh](https://iroh.computer) (v0.97). Enables two BitRouter instances to tunnel LLM API requests over encrypted QUIC connections, authenticated by Ed25519 keypairs.

## Architecture

```
Client -> [BitRouter-A (inbound proxy)]
                 | iroh QUIC tunnel (Ed25519 authenticated)
           [BitRouter-B (outbound proxy)] -> LLM Provider APIs
```

**BitRouter-A** accepts HTTP API requests locally, resolves the model to a P2P peer via the routing table, and forwards the raw request over an iroh QUIC stream. **BitRouter-B** receives the tunneled request, forwards it to its own local HTTP server (reusing all existing auth, guardrails, routing, and streaming logic), and streams the response back through the tunnel.

## Modules

| Module | Purpose |
|---|---|
| `frame` | Wire format: `TunnelRequest` and `TunnelResponseHeader` serialized as length-prefixed JSON over QUIC streams. Request bodies are base64-encoded in the JSON envelope. |
| `endpoint` | `P2pEndpoint`: binds an iroh `Endpoint` from a 32-byte Ed25519 seed, runs the inbound accept loop with peer allow-list enforcement. |
| `inbound` | `InboundHandler`: receives tunneled requests from QUIC streams and forwards them to the local BitRouter HTTP server via localhost `reqwest` call. Handles both streaming (SSE passthrough) and non-streaming responses. |
| `client` | `send_request()`: opens a QUIC connection to a remote peer, sends a `TunnelRequest`, and returns a `TunnelResponse` (complete body or streaming byte stream). |

## Wire Format

Each QUIC bidirectional stream carries one request/response pair.

**Request** (outbound -> inbound):
```
[4 bytes BE: JSON length]
[JSON: TunnelRequest { method, path, headers, body (base64) }]
```

**Response** (inbound -> outbound):
```
[4 bytes BE: JSON length]
[JSON: TunnelResponseHeader { status, headers, streaming }]
```

For non-streaming responses, the body follows as another length-prefixed frame. For streaming (SSE) responses, raw SSE bytes are written directly to the QUIC stream until EOF.

## Identity

The iroh endpoint derives its Ed25519 keypair from a 32-byte seed persisted at `~/.bitrouter/p2p_key`. This seed can be the same as the `MasterKeypair` seed used for Solana identity, ensuring the iroh `EndpointId` shares the same cryptographic root.

## Discovery

Uses iroh's built-in DNS + Pkarr discovery via n0's public relay infrastructure (`dns.iroh.link`). Peers are configured statically by `EndpointId` in `bitrouter.yaml`; iroh handles NAT traversal and relay fallback transparently.

## Security

- **Allow-list enforcement**: inbound connections are accepted only from peers whose `EndpointId` appears in the `p2p.allow_list` config. Empty list = refuse all inbound (outbound-only mode).
- **End-to-end encryption**: all traffic is encrypted via TLS 1.3 (QUIC). Relay servers cannot read tunneled data.
- **Frame size limits**: maximum 64 MiB per frame to prevent unbounded allocations.

## Configuration

```yaml
# Inbound proxy (client-facing)
p2p:
  enabled: true

providers:
  remote-node:
    api_protocol: p2p
    node_id: "<base32 EndpointId>"

models:
  gpt-4o:
    endpoints:
      - provider: remote-node
        model_id: gpt-4o

# Outbound proxy (provider-facing)
p2p:
  enabled: true
  allow_list:
    - "<base32 EndpointId of inbound peer>"
```

## Feature Flag

The P2P feature is gated behind `p2p` in the `bitrouter` binary crate:

```toml
cargo run --features p2p -- serve
```

## Dependencies

- `iroh` 0.97 — QUIC endpoint, connection management, relay/discovery
- `bitrouter-core` — error types, trait definitions
- `reqwest` — localhost HTTP forwarding for inbound handler
- `tokio` / `tokio-stream` — async runtime, stream adapters
- `serde` / `serde_json` — frame serialization
