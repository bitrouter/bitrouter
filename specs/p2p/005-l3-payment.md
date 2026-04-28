# 005 — L3 支付：MPP 在 BitRouter Leg A 中的绑定

> 状态：**v0.9 — 007-02 R1–R11 应用版**。
>
> 本文定义 **Leg A：Consumer ↔ Provider Direct** 的支付 wire。Leg B（PGW↔Provider）的高并发支付控制平面见 [`004-03`](./004-03-pgw-provider-link.md)；Leg C（PGW↔Consumer）不属于 BitRouter 协议范围。
>
> 本版直接覆盖旧的 "PGW 路径与 Pure P2P 共用同一 MPP wire / 订单信封头 / 结算 trailer" 设计。旧订单信封 HTTP 头已删除；旧结算 trailer 正式更名并收敛为 MPP `Payment-Receipt`。

---

## 0. TL;DR

- **Leg A 严格对齐 MPP HTTP Transport**：支付/鉴权失败返回 `402 Payment Required` + `WWW-Authenticate: Payment ...` 多 auth-param challenge；付款请求携带 `Authorization: Payment <base64url(JCS({challenge, source, payload}))>`。
- **Tempo session voucher 使用 EIP-712 + secp256k1 EOA**：`source = did:pkh:eip155:4217:0x...`；voucher 位于 `payload.tempo.voucher`，金额字段是 TIP-20 base units integer string / EIP-712 `uint256`。
- **SSE body 严格保持 OpenAI v1 chat completions shape**：匿名 `data: <json>` 帧、最终 usage chunk、最后 `data: [DONE]`；SSE body 不携任何 BitRouter-specific 字段。
- **结算回执走 `Payment-Receipt` trailer + GET fallback**：receipt schema 对齐 MPP `/protocol/receipts`，外加 BitRouter `order` 扩展；由 Provider ed25519 重新签名。
- **订单上下文是 MPP credential 的 `payload.order` 扩展**：PGW path on Leg A 时可存在；Direct path 无 PGW 时整体省略，由 Provider 自生成 `order_id`。

---

## 1. 适用范围

| Leg | 是否由本文定义 | 支付 wire |
|---|---:|---|
| **A — Consumer ↔ Provider Direct** | ==是== | MPP per-request challenge / credential / receipt |
| **B — PGW ↔ Provider** | 否，见 [`004-03`](./004-03-pgw-provider-link.md) | 独立 Control Connection + cumulative voucher |
| **C — PGW ↔ Consumer** | 否 | PGW 自定义 |

本文中的 R3 / R7 / R8 / R9 / R10 均只适用于 Leg A。若 Leg B 出于 debugging / fallback 需要退回 per-request MPP，也必须按本文 wire 执行。

---

## 2. Transport

Leg A 的应用层运行在 [`003 §6.1`](./003-l3-design.md) 定义的 HTTP/3 over QUIC 上：

```text
ALPN = bitrouter/p2p/0
```

`bitrouter/p2p/0` 的语义已直接重定义为标准 HTTP/3；不保留旧自定义 framing，不新增过渡 ALPN。HTTP headers、status、body、trailers 均按 HTTP/3 标准语义承载。

---

## 3. 响应流规范（OpenAI-compatible SSE）

Leg A streaming response body 必须保持 HTTP/1.1 兼容的 SSE 字节形态，即使底层是 HTTP/3 DATA frame：

```http
HTTP/3 200 OK
Content-Type: text/event-stream; charset=utf-8
Trailer: Payment-Receipt, Payment-Receipt-Sig
```

Normative rules:

1. 所有帧均为匿名 SSE：`data: <json>\n\n`；不得使用 `event:` 字段。
2. 增量内容帧使用 OpenAI v1 chat completions chunk shape：

   ```text
   data: {"id":"chatcmpl-...","object":"chat.completion.chunk","created":...,"model":"...","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}
   ```

3. 最终 usage chunk 使用 OpenAI `stream_options.include_usage` 兼容 shape：

   ```text
   data: {"id":"chatcmpl-...","object":"chat.completion.chunk","created":...,"model":"...","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}
   ```

4. 最后一帧必须是：

   ```text
   data: [DONE]
   ```

5. SSE body ==**不得**==携带 `bitrouter`、`settlement`、`receipt`、`order`、`voucher` 等 BitRouter-specific 扩展字段。
6. streaming 已开始后的业务错误用一帧 OpenAI-compatible error：

   ```text
   data: {"error":{"code":"upstream.timeout","category":"upstream","retriable":true,"message":"upstream timed out"}}

   data: [DONE]
   ```

支付回执不在 SSE body 中出现，见 §6。

---

## 4. Challenge：`402 + WWW-Authenticate: Payment`

Consumer 第一次请求可以不带 `Authorization: Payment`。Provider 返回 MPP challenge：

```http
HTTP/3 402 Payment Required
WWW-Authenticate: Payment id="...", realm="ed25519:<provider_id>", method="tempo", intent="session", request="<base64url(JCS(request_json))>", expires="2026-04-27T00:05:00Z", digest="<sha256-hex(body)>", opaque="<base64url(JCS(opaque_json))>"
Content-Type: application/problem+json
```

字段语义：

| auth-param | 语义 |
|---|---|
| `id` | Provider HMAC-bound challenge id |
| `realm` | 收款 / 验证方身份；Direct Provider 为 `ed25519:<z-base32>` |
| `method` | MPP method，如 `tempo` |
| `intent` | token-based LLM API 必须是 `session` |
| `request` | method-specific challenge body 的 JCS + base64url |
| `expires` | RFC 3339 绝对过期时间 |
| `digest` | 原始 request body 的 SHA-256 hex；空 body 为 SHA-256 empty digest |
| `opaque` | Provider 私有 JCS object 的 base64url 编码 |

`id` 由 Provider 用私钥派生 HMAC key 计算，输入为：

```text
realm|method|intent|request|expires|digest|opaque
```

缺省槽用空字符串；`|` 分隔符固定。Provider 在收到 credential 后必须重算并验证 `id`，防止 challenge auth-param 被 in-flight 篡改。

同一响应可以给多个 `WWW-Authenticate: Payment ...` header，Consumer 从中选择一个 method / intent。支付与鉴权类错误均走本节 402 形态；非支付错误见 §9。

---

## 5. Credential：`Authorization: Payment`

Consumer 按 MPP credential 形态重发请求：

```http
POST /v1/chat/completions HTTP/3
Content-Type: application/json
Authorization: Payment <base64url(JCS(credential_json))>
```

`credential_json`：

```jsonc
{
  "challenge": {
    "id": "...",
    "realm": "ed25519:<provider_id>",
    "method": "tempo",
    "intent": "session",
    "request": "<base64url(JCS(request_json))>",
    "expires": "2026-04-27T00:05:00Z",
    "digest": "<sha256-hex(body)>",
    "opaque": "<base64url(JCS(opaque_json))>"
  },
  "source": "did:pkh:eip155:4217:0x<consumer_eoa>",
  "payload": {
    "tempo": {
      "voucher": { "...": "EIP-712 voucher, see §7" }
    },
    "order": { "...": "optional BitRouter order extension, see §8" }
  }
}
```

Rules:

1. `challenge` 必须逐字段回传 Provider 发出的 auth-param 对象，含 `id`。
2. `source` 在 Tempo session intent 下是 Consumer 的 secp256k1 EOA DID：`did:pkh:eip155:4217:0x...`（testnet 用 `42431`）。若未来使用 BitRouter node identity 作为 source，则必须是 [`001-02 §8.5`](./001-02-terms.md) 的 `<algo>:<z-base32>`。
3. `payload` 由 method-specific 子对象与 BitRouter extension 子对象组成；所有 JSON 签名 / 哈希输入使用 JCS RFC 8785。

---

## 6. `Payment-Receipt`：trailer + GET fallback

### 6.1 Receipt schema

`Payment-Receipt` 的基础字段严格遵循 MPP `/protocol/receipts`：

```jsonc
{
  "challenge_id": "<challenge.id>",
  "method": "tempo",
  "intent": "session",
  "reference": "0x<channel_id>",
  "settlement": {
    "amount": "123456",
    "currency": "0x20c0000000000000000000000000000000000000"
  },
  "status": "succeeded",
  "timestamp": "2026-04-27T00:00:30Z",
  "order": {
    "order_id": "01J...",
    "provider_id": "ed25519:<provider_id>",
    "pgw_id": "ed25519:<pgw_id>",
    "model": "openai/gpt-4o-mini",
    "pricing_policy_hash": "sha256:...",
    "max_input_tokens": 1024,
    "max_output_tokens": 2048,
    "gross_quote_base_units": "130000",
    "provider_share_base_units": "123500",
    "gateway_share_base_units": "6500"
  }
}
```

Rules:

- `challenge_id` 必须等于本次 challenge `id`。
- `reference` 在 Tempo session intent 下等于 channel_id（bytes32 hex）。
- `settlement.amount` 与所有 `*BaseUnits` 字段均为 TIP-20 base units integer string；禁止 decimal、科学计数法、前导零（`"0"` 除外）。
- `settlement.currency` 使用 MPP / Tempo method 的支付资产标识。
- `order` 扩展按 §8；Direct path 无 PGW 时可由 Provider 自生成最小 `{order_id, provider_id, model}`，也可省略 PGW 相关字段。

### 6.2 Trailer 主通道

streaming response 起始 header 必须预声明 trailer：

```http
Trailer: Payment-Receipt, Payment-Receipt-Sig
```

SSE body 结束后发送 HTTP trailer：

```http
Payment-Receipt: <base64url(JCS(receipt_json))>
Payment-Receipt-Sig: <base64url(ed25519_sig)>
```

`Payment-Receipt-Sig` 签名输入为 JCS(receipt JSON)，签名 key 是 Provider `provider_id` 对应 ed25519 私钥。Provider 必须在每次响应结束时重新构造、重新签名整个 receipt；禁止转发 PGW 签名或复用 credential / order signature。

### 6.3 GET fallback

由于浏览器 `fetch`、OpenAI SDK 或反代可能丢弃 trailer，Provider 必须暴露：

```http
GET /v1/payments/receipts/{challenge_id}
```

成功响应：

```http
HTTP/3 200 OK
Content-Type: application/json
Payment-Receipt: <base64url(JCS(receipt_json))>
Payment-Receipt-Sig: <base64url(ed25519_sig)>
```

body 返回同一份 `receipt_json`。Provider 必须保留已结算 receipt ≥ 24h。不存在或尚未结算时返回 `404 application/problem+json`，错误码 `receipt.not_found` 或 `receipt.not_ready`。

---

## 7. Tempo session voucher（R9）

Tempo session intent 的 `payload.tempo.voucher` 使用 EIP-712 typed data + secp256k1 ECDSA。BitRouter ed25519 key 仍用于 node identity、snapshot、receipt、order extension 签名；不用于 Tempo 链上 voucher。

EIP-712 domain:

```jsonc
{
  "name": "TempoStreamChannel",
  "version": "1",
  "chainId": 4217,
  "verifyingContract": "<TempoStreamChannel contract>"
}
```

testnet `chainId` 为 `42431`。Primary type:

```jsonc
{
  "Voucher": [
    { "name": "channel_id", "type": "bytes32" },
    { "name": "cumulative_amount", "type": "uint256" },
    { "name": "nonce", "type": "uint256" },
    { "name": "action", "type": "string" }
  ]
}
```

`action ∈ {"open","top_up","voucher","close"}`。`cumulative_amount` 是 TIP-20 base units，对应 BitRouter JSON 投影中的整数字符串；进入 EIP-712 typed data 时作为 `uint256`。Signer 是 Consumer secp256k1 EOA，DID 形式见 §5 `source`。具体 method details 与 Rust 类型复用见 [`004-02`](./004-02-payment-protocol.md)。EIP-712 typed data 的标准字段（如 `chainId`、`verifyingContract`、`primaryType`）保留外部标准命名，不纳入 BitRouter JSON 字段命名规则。

---

## 8. BitRouter `payload.order` 扩展（R10）

PGW 参与 Leg A MPP credential 构造时，订单上下文放入 `payload.order`：

```jsonc
{
  "order_id": "01J...",
  "provider_id": "ed25519:<provider_id>",
  "pgw_id": "ed25519:<pgw_id>",
  "model": "openai/gpt-4o-mini",
  "pricing_policy_hash": "sha256:<hash>",
  "max_input_tokens": 1024,
  "max_output_tokens": 2048,
  "gross_quote_base_units": "130000",
  "provider_share_base_units": "123500",
  "gateway_share_base_units": "6500",
  "order_sig": "<base64url(ed25519_sig_by_pgw)>"
}
```

Rules:

1. Direct path（无 PGW）下 `payload.order` 整体省略；Provider 自生成 `order_id` 并在 receipt 中反映。
2. `order_sig` 是 PGW `pgw_id` 对 JCS(order object without `order_sig`) 的 ed25519 签名。
3. `gross_quote_base_units == provider_share_base_units + gateway_share_base_units` 必须在 integer domain 严格成立。
4. `pricing_policy_hash` 必须命中 Provider 当前有效 pricing policy。
5. `max_input_tokens` / `max_output_tokens` 是 Provider 服务上限；实际 usage 不得超过，超出前应截断或 streaming 前拒绝。

旧订单信封 HTTP 头不再存在；任何实现不得同时支持两种订单上下文来源。

---

## 9. 错误模型（R4）

| 类别 | wire |
|---|---|
| 支付 credential 缺失 / 失效 / 过期 / 不足额 | `402 Payment Required` + MPP `WWW-Authenticate: Payment ...` challenge |
| 身份 / Registry / 上游 / 链 / transport 等非支付错误 | RFC 9457 `application/problem+json` |
| SSE 已开始后的流内错误 | `data: {"error": {...}}\n\n` 后跟 `data: [DONE]\n\n` |

problem+json 示例：

```json
{
  "type": "https://bitrouter.ai/errors/registry.snapshot_stale",
  "title": "Registry snapshot is stale",
  "status": 409,
  "detail": "Provider snapshot is older than freshness window",
  "code": "registry.snapshot_stale",
  "category": "registry",
  "retriable": true
}
```

错误码注册规则见 [`003` Appendix 协议错误模型](./003-l3-design.md#协议错误模型)。支付 / auth 类错误不使用自定义 `{error:{...}}` HTTP envelope。

---

## 10. Provider 必检项

Leg A Provider 必须至少执行：

| # | 检查 | 失败处理 |
|---|---|---|
| C5 | Tempo voucher channel、nonce、cumulative、collateral 有效 | 402 + 新 challenge |
| C6 | Direct path credential `source` 与付款身份一致；无 PGW 时不得伪造 `payload.order` | 402 / problem+json |
| C7 | `Payment-Receipt.challenge_id == challenge.id`，`reference == channel_id`，`order.order_id` 与 credential `payload.order.order_id` 一致 | `receipt.*` error |
| C8 | `Payment-Receipt-Sig` 必须由 Provider 自身 ed25519 key 签名 | `receipt.signature_invalid` |
| C9 | challenge `digest` 等于实际 request body SHA-256 | 402 + 新 challenge |
| C10 | challenge `expires` 未过期且时钟漂移在 Provider policy 范围内 | 402 + 新 challenge |
| C11 | `payload.order.pricing_policy_hash` 命中当前 pricing policy（存在 PGW order 时） | problem+json `pricing.policy_unknown` |
| C12 | 实际 token 用量 ≤ `max_input_tokens` / `max_output_tokens` | 截断或 problem+json `quota.exceeded` |
| C13 | fee split 等式在 base-unit integer domain 成立 | problem+json `order.fee_split_invalid` |

完整 Direct + PGW 合并表见 [`004-03 §6`](./004-03-pgw-provider-link.md#6-provider-必检项normative)。

---

## 11. 金额表示（R1）

所有结算金额字段使用 token 原生 atomic unit 的 JSON string：

- `payload.tempo.voucher.cumulative_amount`
- `Payment-Receipt.settlement.amount`
- `payload.order.gross_quote_base_units`
- `payload.order.provider_share_base_units`
- `payload.order.gateway_share_base_units`
- future `top_up.amount` / `collateral_base_units`

整数表示规则：

- 正则：`^(0|[1-9][0-9]*)$`
- 禁止小数点、科学计数法、前导零；
- 计算使用 big-int；
- 报价/费率使用 [`004-02`](./004-02-payment-protocol.md) 的 rational `{numerator, denominator}`，唯一舍入点是 `ceil(numerator * usage_units / denominator)`。

---

## 12. 实现清单

1. HTTP/3 stack 支持 response trailer 与 `GET /v1/payments/receipts/{challenge_id}`。
2. MPP challenge parser / serializer 支持多 auth-param，不支持把整个 challenge 折叠进单一 base64url auth-param 的旧形态。
3. Credential JCS canonicalization 与 HMAC-bound challenge id 验证。
4. Tempo EIP-712 secp256k1 voucher 验证。
5. Provider ed25519 `Payment-Receipt-Sig` 生成与验证测试向量。
6. SSE fixture：匿名 OpenAI chunk + final usage chunk + `[DONE]`，且 body 无 BitRouter-specific 字段。
7. `payload.order` extension 签名、fee split、pricing hash、token limit 测试向量。
