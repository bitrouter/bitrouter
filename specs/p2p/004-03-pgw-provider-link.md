# 004-03 — PGW↔Provider Link：Leg B 支付控制平面

> 状态：**v0.3 — 007-02 R11/R12 应用版**。
>
> 本文只规定 **Leg B：PGW ↔ Provider**。三段 leg 总纲见 [`003 §1.4`](./003-l3-design.md)：Leg A (Consumer↔Provider Direct) 严格 MPP + OpenAI SSE；Leg B 是 BitRouter 内部高并发链路；Leg C (PGW↔Consumer) 不属于 BitRouter 协议范围。
>
> 本版直接覆盖旧的 "默认 MPP session + 订单信封头 + per-request voucher 头" 设计。旧订单信封 HTTP 头已被 R10 拒绝；Leg A 的订单上下文迁入 MPP credential `payload.order`，Leg B 只在 Data Connection 上保留 `BR-Order-Ref`。

---

## 0. TL;DR

- **Leg B 使用双连接拓扑**：Data Connection = 标准 HTTP/3 ALPN `h3`，只承载 LLM API stream；Control Connection = 独立 QUIC ALPN `bitrouter/payctl/1`，只承载支付控制帧。
- **两条连接必须独立**（不同 QUIC connection / TLS session）：接受双握手成本，换取 LLM 流与支付控制面的 head-of-line、拥塞、重启、证书轮换隔离。
- **Data Connection 上唯一 BitRouter-specific 字段**是 `BR-Order-Ref: <ulid>`。LLM request / response body 不携 voucher、不携 receipt、不携订单对象、不携任何支付字段。
- **支付通道是长期 BR-internal channel**：PGW 在 Tempo 链上按 [`004-02`](./004-02-payment-protocol.md) / R9 锁 collateral；PGW↔Provider 链下 voucher 使用 BitRouter ed25519 签名，**不是** EIP-712。
- **Provider 在 Leg B 不逐请求验证 pricing / fee split / token limit**：这些规则由 PGW 内部账本与 Control Connection 接管；Provider 只校验对端 PGW、channel、voucher 单调性与 stream completion 对账。

---

## 1. 范围与边界

| Leg | 端点 | 本文是否规定 | 支付模型 |
|---|---|---:|---|
| **A** | Consumer ↔ Provider Direct | 否，见 [`005`](./005-l3-payment.md) | MPP per-request challenge + `Payment-Receipt` |
| **B** | PGW ↔ Provider | ==是== | 独立 Control Connection + 长期累计 voucher |
| **C** | PGW ↔ Consumer | 否 | PGW 自定义（OpenAI / Anthropic / MPP / x402 / SaaS API key 等） |

Consumer 完全不感知本文协议。PGW 可以把 Leg B 上的 LLM byte stream 翻译、转发、包装成任意 Leg C 形态；BitRouter 只保证 Leg B 提供足够的 `order_ref`、usage、voucher 与 channel 状态供 PGW 对账。

---

## 2. 双连接拓扑（normative）

PGW 与 Provider 建立并维护两条独立 QUIC 连接：

| 连接 | ALPN | 生命周期 | 用途 |
|---|---|---|---|
| **Data Connection** | `h3` | 按 HTTP/3 连接池维护，可多 stream 并发 | LLM API 请求与响应；每路 LLM 调用一条 HTTP/3 request stream |
| **Control Connection** | `bitrouter/payctl/1` | PGW↔Provider link 建立时启动，长期保持；idle 后立即重连 | channel open / voucher / stream completion / epoch close / error / keepalive |

==**禁止**==在 Data Connection 内复用额外 bi-stream 承载支付控制面；也禁止在 LLM request / response body 中插入支付帧。Control Connection 断开时，Data Connection 上已在途的 LLM stream 可以继续输出；Provider 必须进入保守阈值模式，直到 Control Connection 恢复并收到足额 voucher，或主动关闭 Data Connection。

---

## 3. Data Connection：LLM stream 约束

Data Connection 是标准 HTTP/3：

```http
POST /v1/chat/completions HTTP/3
Host: <provider>
Content-Type: application/json
BR-Order-Ref: 01J...ULID
```

Normative rules:

1. `BR-Order-Ref: <ulid>` 是 Data Connection 上唯一 BitRouter-specific header。
2. `BR-Order-Ref` 只用于把本路 LLM stream 与 Control Connection 上的 `payment-stream-completed` 帧关联；Provider 不解析其业务含义。
3. Data Connection 不携 `Authorization: Payment`、不携 `WWW-Authenticate: Payment`、不携 `Payment-Receipt`、不携 voucher、不中途插入 BitRouter SSE 事件。
4. LLM response body 建议保持 OpenAI v1 SSE shape，便于 PGW byte-forward 到 Leg C；但 Leg B 私有 wire 不强制对外 SDK 兼容。若 PGW 需要 byte-forward 给 OpenAI-compatible Leg C，则 PGW 必须约束 Provider 输出满足 [`005 §3`](./005-l3-payment.md) 的 SSE body 规则。

---

## 4. Control Connection：长期 channel

### 4.1 Channel 字段

PGW 与 Provider 在 Control Connection 上协商一条长期 BitRouter-internal payment channel：

```jsonc
{
  "channel_id": "0x<32-byte hex>",
  "provider_id": "ed25519:<z-base32>",
  "pgw_id": "ed25519:<z-base32>",
  "asset": "eip155:4217/erc20:0x...",
  "collateral_base_units": "100000000",
  "opened_at": "2026-04-27T00:00:00Z",
  "epoch_duration_sec": 3600
}
```

- `collateral_base_units` 使用 TIP-20 base units 整数字符串。
- PGW 对 Tempo 链上 collateral 的锁定按 [`004-02`](./004-02-payment-protocol.md) 的 EIP-712 / secp256k1 规则执行。
- Leg B 链下 voucher 只在 BitRouter 内部使用，由 `pgw_id` 对应 ed25519 私钥签名；它不提交给 Tempo 合约直接验签。

### 4.2 累计型 voucher

PGW 周期性（按 epoch、金额阈值或 in-flight 风险阈值）推送：

```jsonc
{
  "channel_id": "0x<32-byte hex>",
  "cumulative_amount_base_units": "123456",
  "nonce": 42,
  "signature": "ed25519:<base64url(sig)>"
}
```

签名输入是 JCS RFC 8785：

```json
{ "channel_id": "...", "cumulative_amount_base_units": "...", "nonce": 42 }
```

Provider 必须检查：

- `channel_id` 是当前 active channel；
- `nonce` 严格单调递增；
- `cumulative_amount_base_units` 不回退、不超过 collateral；
- `signature` 可由 `pgw_id` 验证；
- 本地 `expected_cumulative - cumulative_amount_base_units` 不超过双方约定风险阈值。

epoch 结束时 PGW 发送最终 voucher；Provider 可用它向 PGW / 链上结算流程主张应收。链上 close 的具体 Tempo 合约交互不在本文重复，见 [`004-02`](./004-02-payment-protocol.md)。

---

## 5. Control Connection framing

Control Connection 使用 HTTP/3 bi-stream 上的 length-prefixed JCS-JSON frame，==不是 WebSocket==。每帧包含：

```jsonc
{
  "type": "payment-voucher",
  "id": "01J...ULID",
  "payload": { "...": "..." }
}
```

| 帧名 | 方向 | payload |
|---|---|---|
| `channel-open-request` | PGW → Provider | `{ channel_id, asset, collateral_base_units, opened_at, epoch_duration_sec }` |
| `channel-open-ack` | Provider → PGW | `{ channel_id, provider_id, accepted_at, risk_threshold_base_units }` |
| `payment-voucher` | PGW → Provider | `{ channel_id, cumulative_amount_base_units, nonce, signature }` |
| `payment-stream-completed` | Provider → PGW | `{ order_ref, provider_share_base_units, usage, completed_at }` |
| `payment-epoch-close` | PGW → Provider | `{ channel_id, final_cumulative_base_units, final_nonce, signature }` |
| `payment-error` | 双向 | RFC 9457 problem+json object |
| `keepalive` | 双向 | `{ ts }` |

`payment-stream-completed` 由 Provider 在每路 LLM response 完成后发送。`provider_share_base_units` 是 Provider 对本路 stream 的应收，使用 TIP-20 base units 整数字符串；`usage` 至少包含 `{input_tokens, output_tokens, total_tokens}`，可扩展缓存命中等 Provider-internal 统计。

---

## 6. Provider 必检项（normative）

下表统一列出 Direct path 与 PGW path 的 Provider must-check list。路径标记：

- **D** = Direct / Leg A Consumer↔Provider
- **P** = PGW / Leg B PGW↔Provider
- **B** = Both

| # | 规则 | 路径 |
|---|---|---|
| C1 | 入站连接的 endpoint pubkey ∈ Provider snapshot 的 `accepted_pgws` 白名单（`policy=permissioned`）或满足 `policy=open` 的接受流程 | P |
| C2 | 对端 PGW 的 `pgw_id` snapshot 可验证，当前 endpoint 由该 root key 授权 | P |
| C3 | Control Connection channel 中的 `provider_id` 等于本 Provider 的 `provider_id` | P |
| C4 | 业务字段（`pricing_ref` / `model` / `intent`）与本 Provider 当前 snapshot 中的 pricing 项匹配；不接受陈旧 pricing snapshot | B |
| C5 | voucher 验证：channel 与对端身份匹配；`nonce` 严格单调递增；`cumulative` 不回退、不超过 collateral | B |
| C6 | Direct path 上 credential 的 `source` 必须等于发起者支付身份；Direct path 不允许 Leg B 的 `BR-Order-Ref` 替代 MPP credential | D |
| C7 | Leg A `Payment-Receipt` 的 `challenge_id` / `reference` 必须与本次请求 challenge 的 `id` / 通道 `channel_id` 一致；Leg B 用 `payment-stream-completed.order_ref` 与 Data Connection `BR-Order-Ref` 对齐 | B |
| C8 | Leg A `Payment-Receipt` 必须由 Provider 自身签名；Leg B `payment-stream-completed` 必须由 Control Connection 的认证上下文保护 | B |
| C9 | Leg A challenge `digest` 必须等于实际请求 body SHA-256；Leg B 长期控制流模式下不做 per-request MPP digest 校验 | B |
| C10 | Leg A challenge `expires` 必须是未过期 RFC 3339 时间戳；Leg B 长期控制流模式下由 voucher nonce / epoch 控制 replay | B |
| C11 | Leg A credential `payload.order.pricing_policy_hash` 必须命中 Provider 当前有效 pricing policy；Leg B 中该校验转移到 PGW 内部账本 | P |
| C12 | Leg A 实际 token 用量必须 ≤ `payload.order.max_input_tokens` / `max_output_tokens`；Leg B 中该校验转移到 PGW 内部账本或 PGW↔Provider 商务 SLA | B |
| C13 | Leg A `gross_quote_base_units == provider_share_base_units + gateway_share_base_units`；Leg B 中 fee split 由 PGW 账本负责，Provider 不逐请求验证 | P |

Leg B 主路径下，C9 / C10 / C11 / C12 / C13 的 per-request 校验从 Provider hot path 移出；Provider 只基于 Control Connection 的 cumulative voucher 与本地 usage/cost 汇总控制风险。若 Leg B fallback 到 Leg A per-request MPP，则这些规则重新启用。

---

## 7. 已拒绝设计

| 设计 | 结论 | 理由 |
|---|---|---|
| 旧订单信封 HTTP 头 | ==拒绝== | R10 已将订单上下文迁入 Leg A MPP credential `payload.order`；Leg B 用 `BR-Order-Ref` + Control Connection，不在 Data Connection 传完整订单对象 |
| 旧 per-request channel voucher HTTP 头 | ==拒绝== | 支付控制污染 LLM hot path；高并发下增加 header 处理与重放面；R12 改为独立 Control Connection |
| 在 Data Connection 同 QUIC connection 上开独立 bi-stream 做控制面 | ==拒绝== | 仍共享 congestion / TLS / connection-level failure；不能达到支付与 LLM 流隔离目标 |
| Leg B 强制 per-request MPP challenge | ==拒绝主路径== | 高并发 PGW↔Provider 下每路请求多 round-trip；仅保留为 fallback |
| Leg B voucher 使用 EIP-712 | ==拒绝== | 链下内部 voucher 不直接上链验签；使用 BitRouter ed25519 与 node identity 统一 |

---

## 8. 失败处理

| 失败 | 处理 |
|---|---|
| Control Connection 断开 | Provider 进入保守阈值模式；继续已在途 Data stream，但不接受新 `BR-Order-Ref`，直到 Control Connection 恢复 |
| voucher nonce 回退 / signature invalid | Provider 发送 `payment-error`，拒绝新流；严重时关闭 Data Connection |
| cumulative 超过 collateral | Provider 拒绝新流，要求 PGW top up / epoch close |
| `payment-stream-completed` 上报失败 | Provider 重试同 `order_ref`；PGW 按 `order_ref` 幂等处理 |
| Data stream 无对应 `BR-Order-Ref` | Provider 以 RFC 9457 problem+json 拒绝请求 |

---

## 9. 与其他文档的关系

- [`003`](./003-l3-design.md)：定义三段 leg 与 ALPN 总纲。
- [`004-02`](./004-02-payment-protocol.md)：定义 Tempo / MPP pricing、TIP-20 base units、EIP-712 voucher 的 Leg A / 链上侧语义。
- [`005`](./005-l3-payment.md)：定义 Leg A MPP wire、`payload.order`、`Payment-Receipt`。
- [`001-02`](./001-02-terms.md)：定义 `ed25519:<z-base32>` 身份字符串、`BR-Order-Ref`、`bitrouter/payctl/1` 等术语。
