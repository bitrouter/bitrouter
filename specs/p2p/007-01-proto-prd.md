# 007-01 — v0 Prototype 产品需求文档（PRD）

> 状态：**v0.1 — 初版**。本文定义 BitRouter v0 网络的 **Prototype 产品形态**：完整的协议数据格式、完整的网络拓扑、各参与方的极简实现。
>
> 目标：**为 v0 网络做端到端前期验证**——把 [`003`](./003-l3-design.md) / [`004-01`](./004-01-payment-gateway.md) / [`004-02`](./004-02-payment-protocol.md) / [`004-03`](./004-03-pgw-provider-link.md) / [`005`](./005-l3-payment.md) 已规范的协议字段以最小可运行集合实现一遍，跑通"Consumer → Registry → 拨号 Provider → MPP 付款 → 推理"的全流程。
>
> 范围严格限定：
> - ==**只覆盖 v0**==——中心化 Registry、permissioned Provider、Built-in PGW（默认 = BitRouter Cloud 实例）、单 method = Tempo MPP session、token-only pricing、LLM API only。
> - ==**v1 概念全部排除**==——链上 Registry、permissionless announce、横向扩容、密钥轮换、外部 PGW、多 method、charge intent、非 LLM API、自助 announce 等均**不在本文**。
> - ==**两层身份的退化形态**==——本 prototype 阶段允许 `provider_id == endpoint_id`、`pgw_id == endpoint_id`（即每个实体一把密钥兼任 root + endpoint）。snapshot 字段仍按两层书写以保证前向兼容（详见 §3.2）。
>
> 术语：所有命名遵循 [`001-02-terms`](./001-02-terms.md)。

---

## 0. TL;DR

- ==**5 个组件**==：`registry-svc`（HTTPS 只读）、`provider-node`（推理服务端）、`consumer-cli`（客户端）、`pgw-node`（=BitRouter Cloud built-in 实例）、`bitrouter-registry`（git 仓库，PR + CI）。
- ==**1 条端到端流程**==：Consumer 查 Registry → 选 Provider → 经 PGW 路径或 Direct 路径 → MPP `tempo session` 付款 → Provider 返回推理结果 → Receipt。
- ==**4 份协议数据形态**==：Provider snapshot、PGW snapshot、Order Envelope、Channel Voucher。所有字段在 §3 完整列出，无字段省略号。
- ==**3 套二进制**==：`bitrouter-node`（同进程支持 Provider/PGW 角色）、`bitrouter-cli`（Consumer + 运维）、`registry-svc`（HTTPS 只读）。
- 验证目标：完整跑通 Direct 与 PGW 两条路径各 ≥ 100 次推理调用，无错误，账目平衡。

---

## 1. 范围与非目标

### 1.1 In Scope（v0 必须实现）

| 类别 | 项 |
|---|---|
| 网络分层 | L1 (iroh QUIC) + L2 (iroh DNS/pkarr，复用 n0 公共基础设施) + 自托管 relay (`iroh-relay --dev` 或单台公网部署) |
| Registry | git 仓库 + HTTPS 只读 + PR/CI 强制校验（详见 §4） |
| 身份 | 两层身份字段齐备，但允许 `root == endpoint`（退化） |
| Provider | 推理服务端 + snapshot 自我发布（运维手动 PR 进 git）+ 接收 Order Envelope / Voucher |
| PGW | Built-in 形态、唯一实例 = "BitRouter Cloud"；同时承担 Order Envelope 签发、Tempo MPP session channel 维护、收 5% 网关费 |
| Consumer | 查询 Registry、拨号 Provider、出示 MPP Credential、解析 402 Challenge、收 Receipt |
| 支付 method | **仅 `tempo` + `intent: "session"`**；charge / solana / stripe 全部禁用 |
| API | OpenAI 兼容 `/v1/chat/completions`（含流式）；其他端点为 v1+ |
| 路径 | Direct（Consumer → Provider 直付）+ PGW（Consumer → PGW → Provider）两条都跑通 |
| 计价 | token-based（`input_per_mtok` / `output_per_mtok`） |

### 1.2 Out of Scope（明确排除，v1+）

- 链上 / 去中心化 Registry（003 §8.4 列出的 A/B/C/D 方向）
- Permissionless Provider 自助 announce
- 多 endpoint / 横向扩容（006 全部内容）
- 密钥轮换（root key rotation）
- External PGW（非 BitRouter Cloud 的第三方 PGW）
- 多 method（solana session、stripe charge、lightning 等）
- Charge intent
- 非 LLM API（embeddings、image gen、audio 等）
- 模型成本对账 / DAO 治理 / 信誉系统 / 反女巫
- 跨区域横向扩容、CDN、边缘缓存
- 双方自建智能合约（004-01 §4.2.3）
- 链上 attestation NFT / SBT / KYC anchor

### 1.3 成功标准（Acceptance Criteria）

| # | 标准 | 验收方式 |
|---|---|---|
| AC-1 | 至少 2 个 Provider node 注册进 Registry，至少 1 个 PGW node（BitRouter Cloud） | git 仓库可见 + Registry HTTPS 列表返回 |
| AC-2 | Consumer CLI 一行命令完成 Direct 路径推理调用，看到正确的 LLM 输出 | `bitrouter-cli chat --direct ...` 退出码 0 |
| AC-3 | Consumer CLI 一行命令完成 PGW 路径推理调用 | `bitrouter-cli chat --via-pgw ...` 退出码 0 |
| AC-4 | Direct 与 PGW 两条路径各跑 100 次连续调用，0 错误 | 自动化脚本 + 退出码统计 |
| AC-5 | PGW 路径下，账目可对：Σ`gross_quote` = Σ`provider_share` + Σ`gateway_share`，并且 Tempo channel 上链余额 = 累计 voucher delta | 财务对账脚本 |
| AC-6 | Provider snapshot CI 全规则通过（Schema lint + sig + seq + pricing 校验） | GitHub Actions 全绿 |
| AC-7 | Registry 服务从冷启动到对外可查 < 5 秒 | 运维测量 |
| AC-8 | Provider 或 PGW 离线时，Consumer 拨号失败有清晰错误码（不卡死） | 故障注入测试 |

---

## 2. 网络拓扑

```
                        ┌────────────────────────────────────────┐
                        │   bitrouter-registry (git repo, 私有)  │
                        │   PR + CI 强制 schema/sig/seq 校验     │
                        └────────────────┬───────────────────────┘
                                         │  pull (定时 / webhook)
                                         ▼
                        ┌────────────────────────────────────────┐
                        │  registry-svc (HTTPS 只读, 单实例)     │
                        │  GET /v0/providers/{provider_id}       │
                        │  GET /v0/providers (list)              │
                        │  GET /v0/pgws/{pgw_id}                 │
                        │  GET /v0/pgws (list)                   │
                        └─────┬───────────────────────┬──────────┘
                              │ HTTPS                 │ HTTPS
                              │ (snapshot+sig)        │ (snapshot+sig)
                              ▼                       ▼
              ┌──────────────────┐        ┌──────────────────┐
              │  consumer-cli    │        │  pgw-node        │
              │  (Consumer)      │        │  (BitRouter      │
              │                  │        │   Cloud built-in)│
              └─┬─────────┬──────┘        └─────┬────────────┘
                │         │                     │
        ╔═══════╝         ╚════════ PGW path ═══╝
        ║ Direct path                           │
        ║ (iroh QUIC)                           │ (iroh QUIC)
        ║                                       │
        ▼                                       ▼
   ┌──────────────────────────────────────────────────────┐
   │            provider-node (Provider)                  │
   │   /v1/chat/completions  +  Order Envelope 验证       │
   │   +  X-Channel-Voucher 验证（PGW path）              │
   └──────────────────────────────────────────────────────┘

       ↑                                       ↑
       │ MPP tempo session channel             │
       │ (Direct: Consumer ↔ Provider)         │ (PGW: PGW ↔ Provider)
       └───────────────────────────────────────┘
                         │
                         ▼
             ┌─────────────────────────┐
             │   Tempo testnet         │
             │   (chain_id = 4217)     │
             │   USDC = 0x20c0...0000  │
             └─────────────────────────┘
```

**全网组件清单（单实例计数）**：

| 组件 | 实例数（v0） | 部署方 | 说明 |
|---|---|---|---|
| `bitrouter-registry` (git repo) | 1 | BitRouter team | GitHub 私有仓库 |
| `registry-svc` | 1 | BitRouter team | 公网 HTTPS（如 `registry.bitrouter.ai`） |
| `pgw-node` (BitRouter Cloud) | 1 | BitRouter team | 公网部署，跑 `bitrouter-node --role=pgw` |
| `iroh-relay` | 1 | BitRouter team | 自托管，公网部署 |
| `provider-node` | ≥ 2 | 准入合作伙伴 | 各自部署 |
| `consumer-cli` | N/A | 任意用户 | 客户端二进制 |

> ==**v0 不允许 Provider 多实例**==——同一 `provider_id` 在 Registry 中**只能有一个 endpoint**。多实例 / 多区域是 [`006`](./006-horizontal-scaling.md) 的 v1+ 范畴。

---

## 3. 协议数据形态（完整字段，零省略）

### 3.1 通用约定

- 所有 JSON 使用 RFC 8785 JCS canonical 序列化做哈希 / 签名。
- 所有时间 RFC 3339 UTC（如 `2026-04-01T00:00:00Z`）。
- 所有金额十进制字符串（如 `"3.00"`、`"0.000001"`），单位标注于同级字段（`currency`）。
- 所有 ed25519 公钥编码为 `ed25519:<z-base32>` 字符串。
- 所有 ed25519 签名编码为 `ed25519:<z-base32>` 字符串。
- 所有 EVM 地址使用 EIP-55 checksum。
- 所有链上资产用 CAIP-10：`eip155:4217/erc20:0x20c0000000000000000000000000000000000000`。

### 3.2 Provider snapshot（v0 完整模板）

文件路径：`bitrouter-registry/providers/<provider_id>.json`

```jsonc
{
  "provider_id": "ed25519:abc...xyz",
  "operator_id": "partner-acme",
  "status": "active",
  "admitted_at": "2026-04-01",

  "seq": 1,
  "valid_until": "2026-07-01T00:00:00Z",
  "snapshot_hash_alg": "sha256-canonical-json",

  "endpoints": [
    {
      "endpoint_id": "ed25519:abc...xyz",          // v0: 与 provider_id 相同（退化）
      "region": "geo:ap-east-1",
      "node_addr": {
        "home_relay": "https://relay.bitrouter.ai/",
        "direct_addrs": []
      },
      "chain_addrs": [
        { "chain": "tempo", "addr": "eip155:4217:0xabcDef0123456789abcDef0123456789abcDef01" }
      ],
      "capacity": { "concurrent_requests": 4 },
      "alpn": "bitrouter/p2p/0",
      "min_l3_version": 0,
      "max_l3_version": 0,
      "added_at": "2026-04-01"
    }
  ],

  "models": [
    {
      "name": "claude-3-5-sonnet-20241022",
      "context_window": 200000,
      "max_output_tokens": 8192,
      "tokenizer": "anthropic-claude-3",
      "pricing": [
        {
          "scheme":   "token",
          "rates":    { "input_per_mtok": "3.00", "output_per_mtok": "15.00" },
          "protocol": "mpp",
          "method":   "tempo",
          "currency": "0x20c0000000000000000000000000000000000000",
          "recipient": "0xabcDef0123456789abcDef0123456789abcDef01",
          "method_details": { "chain_id": 4217, "fee_payer": true },
          "intent":   "session",
          "min_increment": "0.000001"
        }
      ]
    }
  ],

  "accepted_pgws": {
    "policy": "permissioned",
    "preferred_methods": [
      { "method": "tempo", "asset": "eip155:4217/erc20:0x20c0000000000000000000000000000000000000" }
    ],
    "min_collateral": "100",
    "max_receivable": "1000",
    "min_topup_warning_ratio": 0.2,
    "required_anchors": [],
    "whitelist": ["ed25519:<bitrouter-cloud pgw_id>"]
  },

  "sig": "ed25519:<root sig over canonical_json(snapshot \\ sig)>"
}
```

==**v0 强制约束**==：

- `endpoints[]` **长度恰为 1**（无横向扩容）。
- `endpoints[0].endpoint_id == provider_id`（退化身份）。
- `pricing[]` 每条 entry 必须 `scheme="token"` + `protocol="mpp"` + `method="tempo"` + `intent="session"`。
- `accepted_pgws.policy` 必须是 `"permissioned"`，且 `whitelist` 恰好包含 BitRouter Cloud 的 `pgw_id`。
- `accepted_pgws.required_anchors` 必须为空数组。
- 不允许 `chain_addrs` 中出现非 `"tempo"` 链。

### 3.3 PGW snapshot（v0 完整模板）

文件路径：`bitrouter-registry/pgws/<pgw_id>.json`

```jsonc
{
  "pgw_id": "ed25519:def...uvw",
  "role": "pgw",
  "operator_id": "bitrouter-cloud",
  "status": "active",
  "admitted_at": "2026-04-01",

  "seq": 1,
  "valid_until": "2026-07-01T00:00:00Z",
  "snapshot_hash_alg": "sha256-canonical-json",

  "endpoints": [
    {
      "endpoint_id": "ed25519:def...uvw",          // v0: 与 pgw_id 相同
      "region": "geo:ap-east-1",
      "node_addr": {
        "home_relay": "https://relay.bitrouter.ai/",
        "direct_addrs": []
      },
      "chain_addrs": [
        { "chain": "tempo", "addr": "eip155:4217:0x123456789aBcDef0123456789aBcDef012345678" }
      ],
      "capacity": { "concurrent_requests": 32 },
      "alpn": "bitrouter/p2p/0",
      "min_l3_version": 0,
      "max_l3_version": 0,
      "added_at": "2026-04-01"
    }
  ],

  "fee_rate": "0.05",                               // 5% 网关费
  "consumer_endpoint": "https://cloud.bitrouter.ai",  // Consumer 走 HTTPS 进入 PGW

  "sourced_provider_requirements": {
    "policy": "permissioned",
    "required_models": [],
    "pricing_ceiling": [],
    "min_attestation": [],
    "sla": { "p99_ms": 5000, "monthly_uptime": 0.99 },
    "onboarding_endpoint": "/v1/_pgw/onboard"
  },

  "sig": "ed25519:<root sig over canonical_json(snapshot \\ sig)>"
}
```

==**v0 强制约束**==：

- 同上 `endpoints[]` 长度 1；`endpoint_id == pgw_id`。
- `fee_rate` 固定 `"0.05"`（5%）。
- `consumer_endpoint` 必须是 HTTPS。
- `sourced_provider_requirements.policy` 必须 `"permissioned"`（Provider 由 BitRouter team 手动登记进 PGW 的 curated set，不接受外部 Provider 自助接入）。

### 3.4 Order Envelope（PGW → Provider）

HTTP header（PGW 转发请求时附加；v0 完整字段）：

```
Order-Envelope: <base64url(canonical_json(envelope))>
```

```jsonc
{
  "envelope_version": "1",
  "order_id": "<uuid v4>",
  "provider_id": "ed25519:abc...xyz",
  "pgw_id": "ed25519:def...uvw",
  "provider_pricing_policy_hash": "sha256:<hex>",   // sha256(provider snapshot canonical_json)
  "consumer_request_hash": "sha256:<hex>",          // sha256(inbound HTTP request body)
  "model": "claude-3-5-sonnet-20241022",
  "max_input_tokens": 1024,
  "max_output_tokens": 2048,
  "gross_quote": "0.001500",
  "provider_share": "0.001425",                     // = gross * (1 - fee_rate)
  "gateway_share": "0.000075",                      // = gross * fee_rate
  "currency": "USDC",
  "method": "tempo",
  "intent": "session",
  "session_id": "<channel_id>",
  "expires_at": "<RFC 3339, now + 30s>",
  "sig": "ed25519:<pgw_id root sig>"
}
```

### 3.5 Channel Voucher（PGW → Provider，每请求）

HTTP header：

```
X-Channel-Id:      <channel_id>
X-Channel-Voucher: <jws of voucher (compact serialization)>
```

Voucher payload（JWS protected payload）：

```jsonc
{
  "channel_id": "<tempo channel id>",
  "nonce": 1234,                                    // 严格单调递增
  "cumulative_amount": "0.158400",                  // 累计 USDC
  "asset": "eip155:4217/erc20:0x20c0000000000000000000000000000000000000",
  "payer": "eip155:4217:0x123...",                  // PGW chain_addr
  "payee": "eip155:4217:0xabc...",                  // Provider chain_addr
  "issued_at": "<RFC 3339>"
}
```

JWS header：`{ "alg": "EdDSA", "kid": "ed25519:<pgw payment key>" }`。

==**不变量**==（Provider 端硬校验，违反一律 402）：

- `voucher.cumulative_amount - prev_voucher.cumulative_amount == envelope.provider_share`
- `voucher.nonce > prev_voucher.nonce`
- `voucher.payer == 当前 channel 的 PGW chain_addr`
- `voucher.payee == 当前 Provider snapshot 的 chain_addr`

### 3.6 402 Challenge（Direct 路径下 Provider 直接出；PGW 路径下不出现）

```
HTTP/1.1 402 Payment Required
WWW-Authenticate: Payment realm="ed25519:<provider_id>", method="tempo", intent="session", challenge="<base64url(challenge_body)>"
Content-Type: application/json

{ "error": "payment_required", "detail": "open a tempo session to ed25519:<provider_id>" }
```

`challenge_body` 字段：

```jsonc
{
  "id": "<uuid>",
  "method": "tempo",
  "intent": "session",
  "asset": "eip155:4217/erc20:0x20c0000000000000000000000000000000000000",
  "payee": "eip155:4217:0xabc...",
  "min_collateral": "1.000000",
  "expires_at": "<RFC 3339, now + 60s>"
}
```

### 3.7 Settlement Trailer（Provider 响应尾）

非流式：HTTP trailer。流式（SSE）：终止事件 `event: settlement`。

```jsonc
{
  "order_id": "<同 envelope.order_id>",
  "actual_input_tokens": 987,
  "actual_output_tokens": 1543,
  "actual_amount": "0.001432",
  "voucher_nonce": 1235,
  "voucher_cumulative_amount": "0.159832",
  "provider_sig": "ed25519:<provider_id sig over above fields>"
}
```

---

## 4. Registry 实现

### 4.1 git 仓库布局

```
bitrouter-registry/
├── README.md                    # 准入流程 + PR review checklist
├── known-models.json            # 团队维护的"已知模型名"白名单（用于 CI 校验）
├── schema/
│   ├── provider.schema.json
│   └── pgw.schema.json
├── providers/
│   └── <provider_id>.json       # 一个 Provider 一份 snapshot
├── pgws/
│   └── <pgw_id>.json
└── .github/workflows/
    └── ci.yml                   # PR 触发，跑 §4.2 全部规则
```

### 4.2 CI 规则（"未来合约会做的检查"）

任意 PR 触发；任一失败 → 拒绝合并。

| # | 规则 | 失败码 |
|---|---|---|
| R-1 | JSON Schema lint（按 `schema/*.schema.json`） | `schema_invalid` |
| R-2 | `sig` 验证：`verify_ed25519(canonical_json(snapshot \ sig), provider_id 或 pgw_id, sig)` | `bad_signature` |
| R-3 | `seq` 严格单调：同 `provider_id`/`pgw_id`，新 snapshot 的 `seq` 必须 == 旧 + 1 | `seq_not_monotonic` |
| R-4 | `valid_until` 在 `[now + 7d, now + 365d]` 范围 | `valid_until_out_of_range` |
| R-5 | `endpoints[]` 长度 == 1（v0 限制） | `multi_endpoint_not_allowed_in_v0` |
| R-6 | `endpoint_id == provider_id`（或 `== pgw_id`）（v0 退化） | `two_tier_must_degenerate_in_v0` |
| R-7 | Provider 的 `pricing[]` 全部 (token, mpp, tempo, session) | `pricing_method_not_allowed_in_v0` |
| R-8 | `model.name` 在 `known-models.json` 白名单 | `unknown_model` |
| R-9 | Provider 的 `accepted_pgws.whitelist` 包含且仅包含当前 BitRouter Cloud 的 `pgw_id` | `pgw_whitelist_mismatch_in_v0` |
| R-10 | 删除 snapshot 必须在 PR description 写"reason: <retired/banned/...>" | `delete_without_reason` |

### 4.3 HTTPS 只读服务

| Path | 返回 |
|---|---|
| `GET /v0/providers` | `[ { "provider_id": "...", "models": [...], "status": "active" } ]`（精简列表，便于发现） |
| `GET /v0/providers/{provider_id}` | 完整 snapshot JSON + sig（原文件） |
| `GET /v0/pgws` | 同上精简列表 |
| `GET /v0/pgws/{pgw_id}` | 完整 snapshot JSON + sig |
| `GET /v0/healthz` | `{ "status": "ok", "git_head": "<sha>", "loaded_at": "<RFC 3339>" }` |

服务实现要点：

- 启动时 `git clone --depth=1`，加载到内存。
- 每 60 秒 `git pull`；HEAD 变更 → 重建内存索引。
- ==**服务自身被假设为不可信**==——客户端必须验证返回 snapshot 的 `sig` 才能信任内容。

---

## 5. 各参与方实现要点

### 5.1 `provider-node`

CLI：`bitrouter-node --role=provider --config=provider.toml`

| 模块 | 行为 |
|---|---|
| Identity | 启动时加载 / 生成 ed25519 keypair；密钥文件本地持久化。`provider_id == endpoint_id`（v0） |
| L1/L2 | iroh `Endpoint`（ALPN `bitrouter/p2p/0`）；relay = 自托管 |
| Snapshot 发布 | CLI 子命令 `bitrouter-cli snapshot prepare --provider --models=... --pricing=...`，输出 JSON；运维拷贝到 git 仓库提 PR |
| 路由 | 接收 QUIC 连接 → 解析 HTTP `/v1/chat/completions` |
| Direct 路径 | 无 `Order-Envelope` header → 走 402 Challenge → 等 Consumer 出 Credential 开 channel → 校验 → 推理 |
| PGW 路径 | 有 `Order-Envelope` header → 校验 §3.4 全部字段 + 校验 `pgw_id` 在已开 channel 列表 + 校验 voucher → 推理 → 出 Settlement Trailer |
| 后端 LLM | 直连一个具体 upstream（OpenAI / Anthropic / vLLM 等），由 `provider.toml` 配置 |
| 错误码 | 406 unsupported model / 402 payment required / 409 voucher invalid / 503 capacity exceeded |

### 5.2 `pgw-node`（BitRouter Cloud built-in）

CLI：`bitrouter-node --role=pgw --config=pgw.toml`

| 模块 | 行为 |
|---|---|
| Identity | 同上；`pgw_id == endpoint_id` |
| 入口 | HTTPS 服务 `consumer_endpoint`（如 `https://cloud.bitrouter.ai`） |
| Consumer 鉴权 | v0 简化为 API Key（运维分发）；不规范化进协议层 |
| 选 Provider | 从 Registry pull provider list → 按 model + price ceiling 过滤 → 选最便宜 |
| Channel 维护 | 对每个 Provider 维护一条 Tempo session channel；启动时若不存在则自动开 channel + lock collateral |
| Order Envelope 签发 | 每请求生成 §3.4 envelope，pgw root key 签名 |
| Voucher 累积 | 每请求 `cumulative_amount += provider_share`，nonce++，JWS 签名 |
| Provider 拨号 | iroh QUIC，复用连接（每 (PGW, Provider) 一条长连接） |
| 计费 | Consumer 端按 `gross_quote` 收 USDC（v0 简化：从 Consumer 预付余额扣，不上链）；Provider 端按 voucher 上链结算 |
| 故障 | Provider 拨号失败 → 切换到下一个候选 Provider |

### 5.3 `consumer-cli` / `bitrouter-cli`

```
bitrouter-cli chat --direct  --provider <provider_id> --model <name> -m "你好"
bitrouter-cli chat --via-pgw --pgw <pgw_id>          --model <name> -m "你好"
bitrouter-cli registry list-providers [--model <name>]
bitrouter-cli registry get-provider <provider_id>
bitrouter-cli wallet open-channel --provider <provider_id> --collateral 1.0   # Direct 路径用
bitrouter-cli wallet balance
```

| 模式 | 行为 |
|---|---|
| Direct | 1) Registry 拉 provider snapshot 验 sig；2) iroh dial `endpoint_id`；3) HTTP `POST /v1/chat/completions`；4) 收 402 → 用本地钱包开 / 复用 channel → 出 Credential → 重试；5) 解析 SSE 流 + Settlement |
| via-pgw | 1) Registry 拉 pgw snapshot 验 sig；2) HTTPS POST `<consumer_endpoint>/v1/chat/completions` + API Key；3) 收响应（PGW 已对 Consumer 隐藏 envelope/voucher） |

### 5.4 `registry-svc`

见 §4.3。Go / Rust 任选；< 500 行代码。

### 5.5 `iroh-relay`

直接跑 `iroh-relay --dev` 或公网部署版。**不写代码**——上游开箱即用。

---

## 6. 端到端流程（Happy Path 走查）

### 6.1 Direct 路径

```
1. consumer-cli: bitrouter-cli chat --direct --provider P --model claude-3-5-sonnet-20241022 -m "你好"
2. consumer-cli → registry-svc: GET /v0/providers/P
3. registry-svc → consumer-cli: 200 + provider snapshot + sig
4. consumer-cli: verify_ed25519(snapshot, P, sig) → ok
5. consumer-cli: iroh dial(endpoint_id = P) → QUIC 连接
6. consumer-cli → provider-node: POST /v1/chat/completions { model, messages }
7. provider-node → consumer-cli: 402 + WWW-Authenticate (challenge)
8. consumer-cli: 钱包开 Tempo session channel(payer=consumer, payee=P, collateral=1 USDC)
                  → 等链上确认
9. consumer-cli → provider-node: POST /v1/chat/completions
                  + Authorization: Payment <credential_json>
10. provider-node: 校验 Credential → 推理 → SSE 流 → settlement trailer
11. consumer-cli: 解析 trailer，本地账目更新
```

### 6.2 PGW 路径

```
1. consumer-cli: bitrouter-cli chat --via-pgw --pgw G --model claude-3-5-sonnet-20241022 -m "你好"
2. consumer-cli → registry-svc: GET /v0/pgws/G → 验 sig → 取 consumer_endpoint
3. consumer-cli → pgw-node (HTTPS): POST /v1/chat/completions
                  + X-API-Key + body
4. pgw-node:
   a. 选 Provider P (由 model 过滤；按价格排序)
   b. 校验已有 channel(P) 是否就绪；不就绪 → 开 channel
   c. 生成 Order Envelope (envelope.gross_quote = max_input * input_rate + max_output * output_rate)
   d. 生成 Voucher (cumulative_amount += envelope.provider_share, nonce++)
   e. iroh dial(endpoint_id = P)
   f. POST /v1/chat/completions + Order-Envelope + X-Channel-Voucher + X-Channel-Id
5. provider-node: 校验 envelope + voucher → 推理 → SSE → settlement trailer
6. pgw-node: 收 trailer → 按 actual_amount 修正下一次 voucher 起点 → 把 SSE 转发给 consumer-cli
7. consumer-cli: 收到 LLM 输出
```

---

## 7. 仓库与代码结构

```
bitrouter/                       # 主仓
├── crates/
│   ├── bitrouter-core/          # snapshot 类型、签名、CAIP、iroh wrapper
│   ├── bitrouter-mpp/           # mpp tempo session 适配（薄封装 mpp crate v0.10）
│   ├── bitrouter-node/          # binary: --role=provider | --role=pgw
│   ├── bitrouter-cli/           # binary: chat / registry / wallet
│   └── registry-svc/            # binary: HTTPS 只读服务
├── schema/                      # JSON Schema（与 bitrouter-registry 同步）
├── examples/
│   ├── direct-e2e.sh
│   ├── pgw-e2e.sh
│   └── load-100x.sh
└── docs/                        # 链回 engineering/p2p/ 系列文档

bitrouter-registry/              # 独立 git repo（团队私有）
└── (见 §4.1)
```

---

## 8. 测试与验收

### 8.1 自动化

| 类别 | 项 |
|---|---|
| 单元测试 | snapshot canonical_json + sig 往返；voucher nonce/amount 不变量；envelope 校验 |
| 集成测试 | 两个 provider-node + 一个 pgw-node + registry-svc + 模拟 LLM upstream，全本地起，跑 `direct-e2e.sh` / `pgw-e2e.sh` |
| 负载 | `load-100x.sh` 对 Direct 与 PGW 各跑 100 并发 1 串行的混合场景 |
| 故障注入 | (a) Provider 中途下线；(b) Voucher nonce 倒序；(c) Envelope 签名错误；(d) Registry 服务 5xx；分别期望对应错误码 |

### 8.2 验收清单

按 §1.3 AC-1 ~ AC-8 逐项打勾，全部通过 → 标记 v0 prototype 完成。

---

## 9. 与上游文档的字段映射

| 字段 | 来源 |
|---|---|
| Provider snapshot 结构 | [`003 §2.2`](./003-l3-design.md) |
| `accepted_pgws` | [`004-03 §3.1`](./004-03-pgw-provider-link.md) |
| `pricing[]` 类型 | [`004-02 §3.2`](./004-02-payment-protocol.md) |
| Order Envelope | [`005 §3`](./005-l3-payment.md) + [`004-01 §3.1`](./004-01-payment-gateway.md) |
| Channel Voucher | [`004-03 §2`](./004-03-pgw-provider-link.md) + mpp tempo session |
| 402 Challenge | [`005 §2.4`](./005-l3-payment.md) |
| Settlement Trailer | [`005 §3`](./005-l3-payment.md) |
| Registry CI | [`003 §2.3`](./003-l3-design.md)（v0 子集）|

---

## 10. 开放项（v0 prototype 完成后讨论）

均**不阻塞**本 PRD 的实现：

- Consumer 钱包 UX（Direct 路径下让用户直接管 Tempo USDC，是否要 hosted wallet？）
- Registry 索引服务的高可用（v0 单实例够用）
- PGW 的多 Provider 选路打分（v0 仅按价格排序）
- 监控 / metrics 维度（v0 prototype 阶段记 Prometheus 基础四金）
- 节点二进制的 OS / arch 矩阵（v0 至少 linux-amd64 + linux-arm64 + macos-arm64）

后续工作产物均归入 [`007-02-...`](./) 之后的子文档。
