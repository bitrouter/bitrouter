# 001-04 — P2P Protocol API Reference & Examples

> 状态：**v0.1 — Reference draft**。
>
> 本文把 P2P 协议涉及的数据格式、endpoint、HTTP header / trailer、Control Connection frame 与示例请求响应集中成 API Reference。规范性命名、编码和签名规则以 [`001-03`](./001-03-protocol-conventions.md) 为准；Registry 细节见 [`008-03`](./008-03-bitrouter-registry.md)；Leg A 支付 wire 见 [`005`](./005-l3-payment.md)；Leg B 控制面见 [`004-03`](./004-03-pgw-provider-link.md)。

---

## 0. Common conventions

### 0.1 Type ID

BitRouter-owned format identifiers use:

```text
bitrouter/<namespace>/<name>/<major>
```

Examples:

| Object / channel | Type ID / ALPN |
|---|---|
| Direct ALPN | `bitrouter/direct/0` |
| Session Control ALPN | `bitrouter/session/control/0` |
| Registry aggregate | `bitrouter/registry/0` |
| Registry node item | `bitrouter/registry/node/0` |
| Registry tombstone | `bitrouter/registry/tombstone/0` |
| Order extension | `bitrouter/order/0` |
| Payment receipt | `bitrouter/payment/receipt/0` |
| Tempo voucher wrapper | `bitrouter/tempo/voucher/0` |
| Ed25519 JCS proof | `bitrouter/proof/ed25519-jcs/0` |
| EIP-712 proof | `bitrouter/proof/eip712/0` |

### 0.2 Bytes and identities

| Data | Format |
|---|---|
| BitRouter ed25519 identity | `ed25519:<base58btc(32 bytes)>` |
| SHA-256 digest | `sha256:<base58btc(32 bytes)>` |
| Ed25519 signature field | `<base58btc(64 bytes)>` |
| EVM address / bytes32 / tx hash | `0x...` |
| Tempo EOA signer | `did:pkh:eip155:<chain_id>:0x<40 hex>` |

### 0.3 Signed envelope

```jsonc
{
  "type": "bitrouter/<namespace>/<name>/0",
  "payload": {},
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/<namespace>/<name>/0",
        "signer": "ed25519:<base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

---

## 1. Static Registry

### 1.1 Read endpoint

Consumer reads the full Registry artifact as a raw file:

```http
GET https://raw.githubusercontent.com/bitrouter/bitrouter-registry/main/registry/v0/registry.json
Accept: application/json
```

No GitHub API token, Registry API key, publish credential, or mutation fee is required.

### 1.2 Registry aggregate

```jsonc
{
  "type": "bitrouter/registry/0",
  "generated_at": "2026-04-28T00:00:00Z",
  "source": {
    "repository": "github.com/bitrouter/bitrouter-registry",
    "branch": "main",
    "commit": "<git-sha>"
  },
  "nodes": [
    {
      "type": "bitrouter/registry/node/0",
      "payload": {
        "node_id": "ed25519:<base58btc>",
        "provider_id": "ed25519:<base58btc>",
        "seq": 7,
        "status": "active",
        "valid_until": "2026-07-01T00:00:00Z",
        "endpoints": [],
        "models": [],
        "accepted_pgws": {}
      },
      "proofs": []
    }
  ]
}
```

### 1.3 Registry node item

Source file path:

```text
registry/v0/nodes/ed25519_<base58btc>.json
```

Minimal shape:

```jsonc
{
  "type": "bitrouter/registry/node/0",
  "payload": {
    "node_id": "ed25519:<base58btc>",
    "provider_id": "ed25519:<base58btc>",
    "seq": 7,
    "status": "active",
    "valid_until": "2026-07-01T00:00:00Z",
    "endpoints": [
      {
        "endpoint_id": "ed25519:<base58btc>",
        "status": "active",
        "region": "geo:us-east-1",
        "relay_urls": ["https://relay-us.bitrouter.ai/"],
        "direct_addrs": [],
        "capacity": { "concurrent_requests": 100 },
        "api_surfaces": ["openai_chat_completions"],
        "min_protocol_version": 0,
        "max_protocol_version": 0
      }
    ],
    "models": [
      {
        "model": "claude-3-5-sonnet-20241022",
        "api_surface": "anthropic_messages",
        "pricing": [
          {
            "scheme": "token",
            "protocol": "mpp",
            "method": "tempo",
            "intent": "session",
            "currency": "0x20c0000000000000000000000000000000000000",
            "recipient": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            "method_details": { "chain_id": 4217, "fee_payer": true },
            "rates": {
              "input": { "numerator": "3000000", "denominator": "1000000" },
              "output": { "numerator": "15000000", "denominator": "1000000" }
            }
          }
        ]
      }
    ],
    "accepted_pgws": {}
  },
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/registry/node/0",
        "signer": "ed25519:<provider_base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

### 1.4 Tombstone

```jsonc
{
  "type": "bitrouter/registry/tombstone/0",
  "payload": {
    "node_id": "ed25519:<base58btc>",
    "provider_id": "ed25519:<base58btc>",
    "seq": 8,
    "reason": "retired",
    "effective_at": "2026-04-28T00:00:00Z"
  },
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/registry/tombstone/0",
        "signer": "ed25519:<provider_base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

---

## 2. Leg A: Consumer ↔ Provider Direct

### 2.1 Transport

```text
ALPN = bitrouter/direct/0
Application protocol = HTTP/3
```

### 2.2 Initial request

```http
POST /v1/chat/completions HTTP/3
Host: <provider>
Content-Type: application/json

{"model":"claude-3-5-sonnet-20241022","messages":[{"role":"user","content":"Hello"}],"stream":true}
```

### 2.3 Payment challenge

```http
HTTP/3 402 Payment Required
WWW-Authenticate: Payment id="01J...", realm="ed25519:<provider_base58btc>", method="tempo", intent="session", request="<base64url(JCS(request_json))>", expires="2026-04-27T00:05:00Z", digest="sha256:<base58btc>", opaque="<base64url(JCS(opaque_json))>"
Content-Type: application/vnd.bitrouter.error+json

{
  "type": "bitrouter/error/0",
  "payload": {
    "code": "payment.required",
    "title": "Payment required",
    "status": 402,
    "category": "payment",
    "retriable": true,
    "doc_url": "https://docs.bitrouter.ai/errors/payment.required"
  }
}
```

### 2.4 Credential request

```http
POST /v1/chat/completions HTTP/3
Content-Type: application/json
Authorization: Payment <base64url(JCS(credential_json))>
```

`credential_json`:

```jsonc
{
  "challenge": {
    "id": "01J...",
    "realm": "ed25519:<provider_base58btc>",
    "method": "tempo",
    "intent": "session",
    "request": "<base64url(JCS(request_json))>",
    "expires": "2026-04-27T00:05:00Z",
    "digest": "sha256:<base58btc>",
    "opaque": "<base64url(JCS(opaque_json))>"
  },
  "source": "did:pkh:eip155:4217:0x<consumer_eoa>",
  "payload": {
    "tempo": {
      "voucher": {
        "type": "bitrouter/tempo/voucher/0",
        "payload": {
          "typed_data": {
            "types": {},
            "primaryType": "Voucher",
            "domain": {
              "name": "TempoStreamChannel",
              "version": "1",
              "chainId": 4217,
              "verifyingContract": "0x..."
            },
            "message": {}
          }
        },
        "proofs": [
          {
            "protected": {
              "type": "bitrouter/proof/eip712/0",
              "payload_type": "bitrouter/tempo/voucher/0",
              "signer": "did:pkh:eip155:4217:0x<consumer_eoa>"
            },
            "signature": "<base58btc(65 bytes)>"
          }
        ]
      }
    },
    "order": {
      "type": "bitrouter/order/0",
      "payload": {
        "order_id": "01J...",
        "provider_id": "ed25519:<provider_base58btc>",
        "pgw_id": "ed25519:<pgw_base58btc>",
        "model": "openai/gpt-4o-mini",
        "pricing_policy_hash": "sha256:<base58btc>",
        "max_input_tokens": 1024,
        "max_output_tokens": 2048,
        "gross_quote_base_units": "130000",
        "provider_share_base_units": "123500",
        "gateway_share_base_units": "6500"
      },
      "proofs": [
        {
          "protected": {
            "type": "bitrouter/proof/ed25519-jcs/0",
            "payload_type": "bitrouter/order/0",
            "signer": "ed25519:<pgw_base58btc>",
            "payload_hash": "sha256:<base58btc>"
          },
          "signature": "<base58btc>"
        }
      ]
    }
  }
}
```

Direct path omits `payload.order`; Provider may generate a local `order_id` for receipt correlation.

### 2.5 Streaming success response

```http
HTTP/3 200 OK
Content-Type: text/event-stream; charset=utf-8
Trailer: Payment-Receipt

data: {"id":"chatcmpl-...","object":"chat.completion.chunk","created":1777390000,"model":"...","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-...","object":"chat.completion.chunk","created":1777390000,"model":"...","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}

data: [DONE]
```

HTTP trailer:

```http
Payment-Receipt: <base64url(JCS(receipt_envelope_json))>
```

### 2.6 Payment receipt envelope

```jsonc
{
  "type": "bitrouter/payment/receipt/0",
  "payload": {
    "challenge_id": "01J...",
    "method": "tempo",
    "intent": "session",
    "reference": "0x<channel_id>",
    "settlement": {
      "amount": "123456",
      "currency": "0x20c0000000000000000000000000000000000000"
    },
    "status": "succeeded",
    "timestamp": "2026-04-28T00:00:00Z",
    "order": {
      "order_id": "01J...",
      "provider_id": "ed25519:<provider_base58btc>",
      "pgw_id": "ed25519:<pgw_base58btc>",
      "model": "openai/gpt-4o-mini",
      "pricing_policy_hash": "sha256:<base58btc>",
      "gross_quote_base_units": "130000",
      "provider_share_base_units": "123500",
      "gateway_share_base_units": "6500"
    }
  },
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/payment/receipt/0",
        "signer": "ed25519:<provider_base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

### 2.7 Receipt fallback

```http
GET /v1/payments/receipts/{challenge_id} HTTP/3
Accept: application/json
```

Success:

```http
HTTP/3 200 OK
Content-Type: application/json
Payment-Receipt: <base64url(JCS(receipt_envelope_json))>

{ "...": "same receipt_envelope_json" }
```

Not found / not ready:

```http
HTTP/3 404 Not Found
Content-Type: application/vnd.bitrouter.error+json

{
  "type": "bitrouter/error/0",
  "payload": {
    "code": "receipt.not_ready",
    "title": "Receipt is not ready",
    "status": 404,
    "category": "payment",
    "retriable": true,
    "doc_url": "https://docs.bitrouter.ai/errors/receipt.not_ready"
  }
}
```

---

## 3. Leg B: PGW ↔ Provider

### 3.1 Data Connection

```text
ALPN = h3
```

```http
POST /v1/chat/completions HTTP/3
Host: <provider>
Content-Type: application/json
BR-Order-Ref: 01J...ULID
```

Data Connection does not carry `Authorization: Payment`, `WWW-Authenticate: Payment`, `Payment-Receipt`, voucher objects, or BitRouter-specific SSE events.

### 3.2 Control Connection

```text
ALPN = bitrouter/session/control/0
Frame format = length-prefixed JCS JSON
```

Common frame shape:

```jsonc
{
  "type": "bitrouter/session/payment-voucher/0",
  "id": "01J...ULID",
  "payload": {},
  "proofs": []
}
```

Frame types:

| Type | Direction | Payload |
|---|---|---|
| `bitrouter/session/channel-open-request/0` | PGW -> Provider | `{ channel_id, asset, collateral_base_units, opened_at, epoch_duration_sec }` |
| `bitrouter/session/channel-open-ack/0` | Provider -> PGW | `{ channel_id, provider_id, accepted_at, risk_threshold_base_units }` |
| `bitrouter/session/payment-voucher/0` | PGW -> Provider | cumulative payment voucher envelope |
| `bitrouter/session/payment-stream-completed/0` | Provider -> PGW | `{ order_ref, provider_share_base_units, usage, completed_at }` |
| `bitrouter/session/payment-epoch-close/0` | PGW -> Provider | final cumulative voucher envelope |
| `bitrouter/session/payment-error/0` | both | BitRouter error payload |
| `bitrouter/session/keepalive/0` | both | `{ ts }` |

### 3.3 Payment voucher

```jsonc
{
  "type": "bitrouter/session/payment-voucher/0",
  "id": "01J...ULID",
  "payload": {
    "channel_id": "0x<32-byte hex>",
    "cumulative_amount_base_units": "123456",
    "nonce": 42
  },
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/session/payment-voucher/0",
        "signer": "ed25519:<pgw_base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

### 3.4 Stream completed

```jsonc
{
  "type": "bitrouter/session/payment-stream-completed/0",
  "id": "01J...ULID",
  "payload": {
    "order_ref": "01J...ULID",
    "provider_share_base_units": "123500",
    "usage": {
      "input_tokens": 12,
      "output_tokens": 34,
      "total_tokens": 46
    },
    "completed_at": "2026-04-28T00:00:00Z"
  }
}
```

### 3.5 Payment error

```jsonc
{
  "type": "bitrouter/session/payment-error/0",
  "id": "01J...ULID",
  "payload": {
    "code": "payment.voucher_invalid",
    "title": "Payment voucher is invalid",
    "status": 402,
    "detail": "voucher nonce regressed",
    "category": "payment",
    "retriable": false,
    "doc_url": "https://docs.bitrouter.ai/errors/payment.voucher_invalid"
  }
}
```

---

## 4. Error object

HTTP errors use `bitrouter/error/0` with `Content-Type: application/vnd.bitrouter.error+json`:

```jsonc
{
  "type": "bitrouter/error/0",
  "payload": {
    "code": "registry.snapshot_stale",
    "title": "Registry snapshot is stale",
    "status": 409,
    "detail": "Provider snapshot is older than freshness window",
    "category": "registry",
    "retriable": true,
    "doc_url": "https://docs.bitrouter.ai/errors/registry.snapshot_stale"
  }
}
```

Streaming errors after SSE has started use an OpenAI-compatible error chunk:

```text
data: {"error":{"code":"upstream.timeout","title":"Upstream timed out","detail":"upstream timed out","category":"upstream","retriable":true,"doc_url":"https://docs.bitrouter.ai/errors/upstream.timeout"}}

data: [DONE]
```
