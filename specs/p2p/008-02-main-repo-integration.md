# 008-02 — P2P 协议集成到主 `bitrouter` 仓库

> 状态：**v0.3 — 主仓库静态 Registry 集成设计**。
>
> 本文负责主仓库 [`bitrouter/bitrouter`](https://github.com/bitrouter/bitrouter) 的代码集成。正式环境部署见 [`008-01`](./008-01-network-deployment.md)；公开 Registry 数据仓库见 [`008-03`](./008-03-bitrouter-registry.md)。

---

## 0. 结论

主 `bitrouter` 仓库新增默认启用的 `p2p` feature：

```toml
default = [
    "cli",
    "tui",
    "sqlite",
    "tempo",
    "mcp",
    "rest",
    "p2p",
]
p2p = ["dep:bitrouter-p2p", "tempo", "bitrouter-api/payments-tempo"]
```

集成形态：

1. 新增一个 companion crate：`bitrouter-p2p`。
2. `bitrouter` binary 同时支持 Consumer role 与 Provider role。
3. `ApiProtocol::P2p` 直接写入 `bitrouter-core`，不受 `p2p` feature 隐藏。
4. 第一阶段只做 Direct Leg A；Leg B / PGW 后置。
5. Leg A 第一阶段支持 OpenAI-compatible Chat Completions 与 Anthropic-compatible Messages。
6. `Payment-Receipt` fallback 使用数据库持久化。
7. Registry client 指向 [`bitrouter-registry`](./008-03-bitrouter-registry.md) 的 GitHub raw `registry.json`；Registry 是去中心化状态源的 v0 公开静态替代，不做服务端查询或准入。
8. Provider 侧 CLI 只导出可提交 PR 的 signed registry file item / tombstone；不调用 publish API，也不支付 registry mutation gas fee。

---

## 1. 主仓库约束

### 1.1 Crate creation rule

`bitrouter-p2p` 满足主仓库新增 crate 规则，因为它引入重依赖树：

- `iroh` / relay / endpoint discovery；
- `h3` / HTTP/3 over iroh adapter；
- pkarr / NodeAddr / relay map；
- 长连接、并发 stream、网络 lifecycle runtime；
- MPP client/server 与 Tempo session 组合能力。

这些依赖不应进入 `bitrouter-core`、`bitrouter-api` 或默认 SDK 编译路径。`p2p` 默认启用，但仍可被自定义 build 关闭以移除依赖树。

### 1.2 禁止重复造轮子

主仓库不得恢复：

- 自写 MPP core；
- mock chain / mock Tempo client runtime；
- JWS voucher；
- `Order-Envelope`；
- settlement SSE event；
- HTTP/1 stream shim over QUIC。

Direct Leg A 必须继续以 upstream / forked MPP SDK 为基础。

---

## 2. Workspace 与 crate 分配

### 2.1 `bitrouter-p2p`

建议模块：

```text
bitrouter-p2p/
├── src/
│   ├── lib.rs
│   ├── config.rs
│   ├── identity.rs
│   ├── registry_client.rs
│   ├── transport/
│   │   ├── mod.rs
│   │   ├── iroh.rs
│   │   └── h3.rs
│   ├── consumer/
│   │   ├── mod.rs
│   │   └── model.rs
│   ├── provider/
│   │   ├── mod.rs
│   │   └── server.rs
│   ├── payment/
│   │   ├── mod.rs
│   │   ├── client.rs
│   │   └── server.rs
│   └── error.rs
```

约束：

- 可以依赖 `bitrouter-core`、`bitrouter-config`、`bitrouter-api`、`mpp-br`、`iroh`、`h3`、Tempo SDK / alloy。
- 不能依赖 `bitrouter` binary crate。
- 第一阶段不拆出 `bitrouter-p2p-core`、`bitrouter-p2p-h3`、`bitrouter-p2p-registry`。
- 原型 `bitrouter-h3` 的实现迁入 `transport/h3.rs`，边界是适配 upstream h3 trait，不扩展成自定义协议栈。

### 2.2 `bitrouter-core`

确定进入 `bitrouter-core`：

- `ApiProtocol::P2p`；
- P2P provider route target 的 transport-neutral 标识；
- root identity 字符串解析（若主仓库其他模块也要验证 `ed25519:<base58btc>`）。

不进入 `bitrouter-core`：

- iroh endpoint；
- NodeAddr；
- H3 session；
- MPP challenge runtime state；
- Tempo escrow RPC client。

### 2.3 `bitrouter-config`

`bitrouter-config` 负责 YAML schema，不承担网络 runtime。

建议新增：

```rust
pub struct P2pConfig {
    pub enabled: bool,
    pub identity: P2pIdentityConfig,
    pub registry: P2pRegistryConfig,
    pub consumer: P2pConsumerConfig,
    pub provider: P2pProviderConfig,
}
```

provider-level target：

```rust
pub struct ProviderConfig {
    pub api_protocol: Option<ApiProtocol>,
    pub p2p: Option<P2pProviderTargetConfig>,
    // existing fields...
}
```

`bitrouter-config` 不需要 `p2p` feature；配置字段只是 serde 类型，不引入重依赖。

### 2.4 `bitrouter-api`

第一阶段不把 P2P listener 放入 `bitrouter-api`，因为 P2P Provider 使用 HTTP/3 over iroh，不是 Warp HTTP server。

但应抽出 transport-neutral handler，避免 `bitrouter-p2p` 复制 OpenAI / Anthropic request 转换逻辑：

```text
bitrouter-api
└── router/
    ├── openai/chat/
    │   ├── filters.rs
    │   ├── service.rs
    │   └── convert.rs
    └── anthropic/messages/
        ├── filters.rs
        ├── service.rs
        └── convert.rs
```

原则：

- Warp filter 留在 `bitrouter-api`。
- “request + RoutingTable + LanguageModelRouter → response stream”的逻辑抽为 service。
- service 不依赖 iroh / h3。
- MPP server-side payment gate 继续复用 `bitrouter-api::mpp::PaymentGate` / `MppState`，必要时补 transport-neutral `PaymentDecision`。

### 2.5 `bitrouter` binary

`bitrouter` 是 assembly 层：

- `runtime::Router` 在 `ApiProtocol::P2p` 下构造 `bitrouter_p2p::consumer::P2pLanguageModel`。
- `ServerPlan::serve()` 在 HTTP server 旁边启动 P2P Provider listener。
- shutdown / reload 与现有 runtime lifecycle 对齐。
- 未启用 `p2p` feature 时，`ApiProtocol::P2p` 仍可解析，但构造 adapter 时返回明确错误：`bitrouter was built without p2p feature`。

---

## 3. 配置设计

### 3.1 节点级 P2P 配置

```yaml
p2p:
  enabled: true

  identity:
    key_file: p2p/identity.key

  registry:
    raw_url: https://raw.githubusercontent.com/bitrouter/bitrouter-registry/main/registry/v0/registry.json
    cache_dir: p2p/registry-cache
    refresh_interval_secs: 300

  consumer:
    enabled: true

  provider:
    enabled: true
    listen_addr: 0.0.0.0:0
    relay_urls:
      - https://relay-us.bitrouter.ai/
      - https://relay-eu.bitrouter.ai/
    expose_models:
      claude-3-5-sonnet-20241022: default
```

### 3.2 P2P Provider 作为上游供应方

```yaml
providers:
  remote-sonnet:
    api_protocol: p2p
    p2p:
      provider_id: ed25519:...
      endpoint_id: ed25519:...
      model: claude-3-5-sonnet-20241022
      api_surface: openai_chat_completions
      payment:
        method: tempo
        intent: session

models:
  sonnet-p2p:
    strategy: priority
    endpoints:
      - provider: remote-sonnet
        model_id: claude-3-5-sonnet-20241022
```

### 3.3 收款配置复用现有 MPP

```yaml
mpp:
  enabled: true
  realm: "BitRouter P2P Provider"
  secret_key: ${MPP_SECRET_KEY}
  networks:
    tempo:
      recipient: "0x..."
      escrow_contract: "0x..."
      rpc_url: "https://rpc.tempo.xyz"
      currency: "0x20c0000000000000000000000000000000000000"
      fee_payer: false
      close_signer: ${TEMPO_CLOSE_SIGNER}
```

P2P Provider 不定义第二套支付配置。

---

## 4. Registry client 语义

Registry 在主仓库集成中被视为“去中心化状态的公开静态替代”：

1. Registry 数据由 `bitrouter-registry` public GitHub repository 中的 signed node item 与 committed aggregate `registry/v0/registry.json` 表达。
2. `bitrouter-p2p` Consumer client 只读取 raw `registry.json`，本地验证 schema、Provider proof、`seq`、`valid_until`、status、pricing 与 endpoint 格式。
3. Registry **不做准入**：不判断某 Provider 是否“被允许经营”、是否 KYC、是否有商业合同、是否进入 curated set。
4. 准入、商业关系、风控、发票、客户入口等放到未来 BitRouter Cloud PGW 或其他 PGW 中完成。
5. Consumer 从 Registry 得到“provider 自签并通过公开 PR 合并的广告状态”；是否信任该 Provider，由本地策略、PGW、allowlist、reputation 或未来机制决定。
6. Registry 实现见 [`008-03`](./008-03-bitrouter-registry.md)：v0 不使用 Supabase、Next.js、Vercel、HTTP publish API 或 query API。

### 4.1 Raw fetch / cache

客户端行为：

1. 对 `p2p.registry.raw_url` 发起普通 HTTP GET。
2. 使用 `ETag` / `Last-Modified` 做 conditional request。
3. 下载后先完整校验，再替换本地 last-known-good cache。
4. 校验失败时保留旧 cache 并显式告警。
5. 所有 model / region / pricing / trust policy 查询都在本地完成。

### 4.2 Provider item export

Provider 侧命令只生成文件，不发布到服务：

```bash
bitrouter p2p registry item export
bitrouter p2p registry tombstone export
```

典型流程：

1. CLI 从本地 P2P identity 与 provider config 生成 node item。
2. CLI 使用 Provider root key 对 canonical JSON 签名。
3. 运营方把输出文件放入 `bitrouter-registry/registry/v0/nodes/` 并提交 PR。
4. 修改 node 时提高 `seq` 并重新签名。
5. shutdown 可以导出 `status: disabled` item 或 signed tombstone。

主仓库不实现 `registry login`、`registry publish`、registry mutation 402 支付或 Registry 服务端 credential 管理。

---

## 5. P2P API protocol

`api_protocol: p2p` 是主仓库本地路由层的 provider adapter 标记，不是 Leg A wire header。

第一阶段 `api_surface`：

| `api_surface` | Leg A HTTP/3 path | 语义 |
|---|---|---|
| `openai_chat_completions` | `POST /v1/chat/completions` | OpenAI Chat Completions；streaming 使用匿名 SSE `data: ...` + `data: [DONE]` |
| `anthropic_messages` | `POST /v1/messages` | Anthropic Messages；streaming 以主仓库 Anthropic handler 的兼容语义为准 |

Wire：

```http
POST /v1/chat/completions HTTP/3
Host: <provider_id>
BR-Protocol-Version: 0
Content-Type: application/json
Authorization: Payment <base64url(JCS(credential_json))>
```

共同规则：

1. ALPN 固定为 `bitrouter/direct/0`，内容是标准 HTTP/3。
2. 第一次请求可以不带 `Authorization: Payment`；Provider 返回 `402 + WWW-Authenticate: Payment ...`。
3. Consumer 用同一个 request body 生成 / 绑定 MPP credential 后重发。
4. `BR-Protocol-Version` 是 L3 版本协商头；缺省按 `0` 处理。
5. payment wire 对两种 API surface 完全相同。
6. `/v1/responses` 后置为第三个 `api_surface`。

---

## 6. Runtime 设计

### 6.1 Consumer role

Consumer role 表现为一个 `LanguageModel` adapter：

```rust
pub struct P2pLanguageModel {
    target: P2pProviderTarget,
    endpoint_pool: P2pEndpointPool,
    payment: P2pPaymentClient,
}
```

流程：

1. 从 Registry cache / config 解析 Provider endpoint。
2. 建立 HTTP/3 over iroh 连接。
3. 发送 LLM request。
4. 遇到 402 后解析 MPP challenge。
5. 使用 OWS wallet / `mpp-br` 生成 Tempo session credential。
6. 重发同一 request。
7. 解析 SSE / response。
8. 校验并缓存 `Payment-Receipt`。

### 6.2 Provider role

Provider role 是后台 task：

```rust
pub struct P2pProviderRuntime<R, T> {
    table: Arc<T>,
    router: Arc<R>,
    payment_gate: Arc<dyn PaymentGate>,
    identity: P2pIdentity,
    transport: H3IrohServer,
}
```

流程：

1. 收到 HTTP/3 request stream。
2. 校验 path、method、`BR-Protocol-Version`。
3. 缺 payment credential 时返回 MPP 402。
4. 验证 credential 后调用本机 `LanguageModelRouter`。
5. streaming body 不插入 BitRouter-specific event。
6. 响应结束写 `Payment-Receipt` trailer。
7. receipt fallback 写数据库。

---

## 7. `Payment-Receipt` fallback 数据库

fallback endpoint：

```http
GET /v1/payments/receipts/{challenge_id}
```

三类标识：

| 标识 | 计算 / 来源 | 用途 |
|---|---|---|
| `challenge_id` | MPP challenge `id` | lookup key |
| `receipt_hash` | `sha256(JCS(receipt_envelope_json))`，表示为 `sha256:<base58btc>` | 内容完整性、幂等缓存、日志关联 |
| `Payment-Receipt.proofs[]` | Provider `provider_id` 对 receipt envelope 的 ed25519-JCS proof | 真实性与不可抵赖性 |

数据库表建议：

```text
p2p_payment_receipts
├── challenge_id TEXT PRIMARY KEY
├── provider_id TEXT NOT NULL
├── endpoint_id TEXT NULL
├── channel_id TEXT NULL
├── receipt_hash TEXT NOT NULL
├── receipt_envelope_json JSON/TEXT NOT NULL
├── receipt_envelope_jcs_b64 TEXT NOT NULL
├── receipt_proof JSON/TEXT NOT NULL
├── created_at TIMESTAMP NOT NULL
└── expires_at TIMESTAMP NOT NULL
```

索引：

- primary key：`challenge_id`；
- unique / secondary：`receipt_hash`；
- secondary：`provider_id, created_at`。

Consumer 校验：

1. 解析 body / header 中的 receipt。
2. 重新计算 `sha256(JCS(receipt_envelope_json))`。
3. 校验 `receipt.payload.challenge_id == challenge_id`。
4. 用 Provider snapshot 中的 `provider_id` 验证 `Payment-Receipt.proofs[]`。
5. 校验 channel_id、settlement amount、status。

---

## 8. 分阶段实施

| Phase | 内容 |
|---|---|
| 0 | `ApiProtocol::P2p`、config schema、feature wiring、禁用 feature 错误 |
| 1 | `bitrouter-p2p` Consumer adapter |
| 2 | P2P Provider listener + Direct Leg A |
| 3 | Static Registry raw client + item export/sync/verify CLI |
| 4 | Tempo localnet + two-node E2E |
| 5 | Leg B / PGW Control Connection |

---

## 9. 验收标准

| 编号 | 标准 |
|---|---|
| INT-1 | 默认 feature 包含 `p2p`；自定义关闭后不编译 iroh / h3 / pkarr |
| INT-2 | `ApiProtocol::P2p` 位于 `bitrouter-core` |
| INT-3 | `api_protocol: p2p` 可路由到 `P2pLanguageModel` |
| INT-4 | 未启用 `p2p` feature 时 runtime 给出明确错误 |
| INT-5 | Direct Leg A 覆盖 `/v1/chat/completions` 与 `/v1/messages` |
| INT-6 | 402 → credential → retry → SSE → receipt 通过 |
| INT-7 | receipt fallback 使用数据库表持久化 |
| INT-8 | P2P Provider 复用现有 `LanguageModelRouter`，不复制 provider adapter |
| INT-9 | P2P payment 复用 `mpp-br` / OWS / Tempo backend |
| INT-10 | Registry client 可读取 raw `registry.json`、验证 proof / `valid_until` / status，并本地筛选 endpoint |
| INT-11 | Provider CLI 可导出 signed node item / tombstone；主仓库不实现 Registry publish API 或 mutation gas fee |
| INT-12 | `cargo fmt -- --check`、`cargo clippy --all-features`、`cargo test --all-features` 通过 |
