# 001-03 — 协议版本、编码与签名规范

> 状态：**v0.1 — 统一签名与编码约定**。
>
> 本文统一 P2P 文档中的 BitRouter 自有格式标识、opaque bytes 编码与签名对象形态。它取代早期 `schema_version`、dotted version、inline `signature` / `sig` / `order_sig`、`ed25519:<z-base32>`、`sha256:<hex>` 与 `Payment-Receipt-Sig` 等分散约定。

---

## 0. 结论

BitRouter 自有协议只保留五条全局规则：

1. 所有 BitRouter 自有格式标识都叫 `type`，值形如 `bitrouter/<namespace>/<name>/<major>`。
2. 所有 BitRouter 自有签名对象都是 `{ type, payload, proofs[] }`。
3. 所有 BitRouter 自有 opaque bytes 都用 `base58btc`，无 multibase 前缀。
4. 所有 BitRouter canonical wire error 都是 `bitrouter/error/0`。
5. 外部协议保持外部标准，只在 BitRouter 边界做 wrapper / projection；RFC 9457 Problem Details 只作为可选外部投影，不是 P2P canonical wire。

---

## 1. BitRouter Type ID

所有 BitRouter 自有对象格式、ALPN、control frame、proof profile 都使用同一种 type ID：

```text
bitrouter/<namespace>/<name>/<major>
```

规则：

- 全小写 ASCII。
- `/` 分层。
- segment 内用 `-` 分词。
- 最后一段是 breaking major version number，例如 `/0`。
- 非 breaking 字段扩展不改 type。
- `scheme` 只保留给 payment pricing 里的计费方案（例如 `scheme: "token"`），不用于签名或协议版本。

示例：

| 用途 | Type ID |
|---|---|
| Direct ALPN | `bitrouter/direct/0` |
| Session Control ALPN | `bitrouter/session/control/0` |
| Registry aggregate | `bitrouter/registry/0` |
| Registry node item | `bitrouter/registry/node/0` |
| Registry tombstone | `bitrouter/registry/tombstone/0` |
| Payment receipt | `bitrouter/payment/receipt/0` |
| Order extension | `bitrouter/order/0` |
| Error object | `bitrouter/error/0` |
| Session payment voucher | `bitrouter/session/payment-voucher/0` |
| Session epoch close | `bitrouter/session/payment-epoch-close/0` |
| Ed25519 + JCS proof | `bitrouter/proof/ed25519-jcs/0` |
| EIP-712 proof | `bitrouter/proof/eip712/0` |

---

## 2. base58btc bytes encoding

所有 BitRouter 自有 opaque bytes 使用 Bitcoin base58 alphabet：

```text
123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz
```

规则：

1. 不使用 multibase `z` 前缀。
2. 不使用 z-base32。
3. 不使用 base64url 表达 BitRouter 自有公钥、签名或 digest。
4. leading zero bytes 按 Bitcoin base58 规则编码为 leading `1`。
5. verifier 必须拒绝非法字符、非 canonical encoding 与空串。

| 数据 | 字符串形式 |
|---|---|
| Ed25519 public key | `ed25519:<base58btc(32 bytes)>` |
| Ed25519 signature | `<base58btc(64 bytes)>` |
| SHA-256 digest | `sha256:<base58btc(32 bytes)>` |
| secp256k1 EIP-712 signature in BitRouter wrapper | `<base58btc(65 bytes)>` |

外部标准保持原格式：EVM address / tx hash / bytes32 仍为 `0x...`；DID PKH 仍为 `did:pkh:eip155:<chain_id>:0x...`；MPP `Authorization: Payment` 仍使用上游规定的 base64url credential token。

---

## 3. Signed Object Envelope

所有 BitRouter 自有签名对象统一为：

```jsonc
{
  "type": "bitrouter/<namespace>/<name>/<major>",
  "payload": {
    "...": "business fields"
  },
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/<namespace>/<name>/<major>",
        "signer": "ed25519:<base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

Normative rules:

1. `payload` 是唯一被签业务对象；不得包含自己的 `signature`、`sig`、`order_sig`。
2. `proofs[]` 可以有多个 proof；每个 proof 独立验证。
3. `proofs[].protected` 参与签名；`signature` 不参与签名。
4. `proofs[].protected.payload_type` 必须等于 envelope top-level `type`。
5. `proofs[].protected.payload_hash` 必须等于 `sha256(JCS(payload))`，digest bytes 用 base58btc。
6. `proofs[].protected.signer` 必须按业务上下文匹配预期身份，例如 Registry node item 匹配 `payload.provider_id`，order 匹配 `payload.pgw_id`。

---

## 4. BitRouter Error Object

BitRouter canonical wire error object 统一为：

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

Normative rules:

1. `type` 固定为 `bitrouter/error/0`；不得在 canonical wire 中使用 URL-based error `type`。
2. `payload.code` 是机器分支与监控聚合的稳定错误码，格式为 `<domain>.<reason>`，例如 `registry.snapshot_stale`。
3. `payload.doc_url` 是人读文档 URL；不得把文档 URL 放入 top-level `type`。
4. HTTP error response 使用 `Content-Type: application/vnd.bitrouter.error+json`。若某个外部 HTTP gateway 必须兼容 RFC 9457，可把 `bitrouter/error/0` 投影为 `application/problem+json`，但该投影不是 P2P canonical wire，客户端不得依赖 RFC 9457 `type` URI 做协议分支。
5. `payload.status` 必须等于 HTTP status code；Session Control frame 内的 error payload 也保留 `status`，用于日志与跨 transport 映射。
6. `payload.category` 是粗粒度分类；`payload.retriable` 表示立即/短期重试是否可能成功。
7. Streaming 已开始后的 SSE 错误保持 OpenAI-compatible `data: {"error": {...}}` 外形，但 `error` 子对象字段应与本节 `payload` 对齐；SSE 错误不携带 top-level `type`。
8. `bitrouter/error/0` 默认不签名；若未来需要可审计错误报告，应另定义 signed envelope，而不是向 error payload 内塞 `signature`。

---

## 5. Ed25519 + JCS proof

`bitrouter/proof/ed25519-jcs/0` 的签名输入是：

```text
bitrouter-signature-input/0\n
JCS({
  "type": envelope.type,
  "payload": envelope.payload,
  "protected": proof.protected
})
```

其中 JCS 是 RFC 8785 JSON Canonicalization Scheme。

验证步骤：

1. parse envelope。
2. 检查 top-level `type` 是当前位置允许的 type ID。
3. 检查 `proof.protected.type == "bitrouter/proof/ed25519-jcs/0"`。
4. 检查 `proof.protected.payload_type == envelope.type`。
5. 计算 `sha256(JCS(envelope.payload))`，与 `proof.protected.payload_hash` 比较。
6. 用 `proof.protected.signer` 解出 ed25519 public key。
7. 按本节 signing input 验证 `proof.signature`。
8. 执行业务身份绑定、`seq`、`valid_until`、nonce、channel 等上下文规则。

---

## 6. EIP-712 proof

Tempo / EVM 链上 voucher 继续使用 EIP-712 typed structured data；BitRouter 不改变 EIP-712 的 hashStruct / domain separator / wallet signing 规则。

外层仍使用 BitRouter envelope：

```jsonc
{
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
        "signer": "did:pkh:eip155:4217:0x..."
      },
      "signature": "<base58btc(65 bytes)>"
    }
  ]
}
```

EIP-712 标准字段（`types`、`primaryType`、`domain`、`message`、`chainId`、`verifyingContract`）保留外部标准命名，不受 BitRouter snake_case 规则约束。`message` 内 BitRouter 业务字段继续使用 snake_case。

---

## 7. 禁止的新格式

新协议文档与实现不得再引入：

- `schema_version`
- dotted type，例如 `bitrouter.registry.node.v0`
- proof `scheme` 字段
- `br_jws_jcs_v1`
- inline `signature`
- inline `sig`
- `order_sig`
- `signature.value`
- `signature.key_id`
- `ed25519:<z-base32>`
- `ed25519:<base64url(signature)>`
- BitRouter 自有 digest 的 `sha256:<hex>`
- canonical wire 中的 URL-based error `type`
- `application/problem+json` 作为 P2P canonical error media type
- "sign object without signature field" 规则
