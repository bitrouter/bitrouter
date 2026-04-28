# 008-03 — `bitrouter-registry`：v0 网络公开静态注册表

> 状态：**v0.3 — Public repository / raw file registry 设计**。
>
> 本文定义 v0 P2P 网络的公开 Registry 形态：一个公开 GitHub 仓库、一组可审查的 registry file item、一个可通过 GitHub raw 直接读取的完整 registry 文件。它不是服务、不是数据库、不是 API、不是 DHT，也不是链上 Registry。正式网络部署见 [`008-01`](./008-01-network-deployment.md)；主仓库集成见 [`008-02`](./008-02-main-repo-integration.md)。

---

## 0. 结论

v0 不运行任何 `bitrouter-registry` 服务。Registry 只是一个公开数据仓库：

- Provider / node 运营方通过 PR 创建、修改或删除自己的 registry file item；
- 仓库 CI 只做 schema、签名、状态机、pricing、endpoint 等静态校验；
- 维护者 merge PR 只代表“该变更符合公开仓库格式与反垃圾规则”，不代表准入、背书、KYC、商业合同或 curated provider set；
- Consumer 不调用 Registry API，只读取 GitHub raw 上的完整 registry 文件，并在本地过滤 model、region、pricing、endpoint；
- Provider 不调用 publish API，不支付 registry mutation gas fee，不需要 Registry 账号；
- 所有可验证状态仍由 Provider root key 签名，GitHub 只是 v0 的公开分发与审查通道。

推荐的公开读取入口类似 `models.dev`：

```text
https://raw.githubusercontent.com/bitrouter/bitrouter-registry/main/registry/v0/registry.json
```

实现上只需要：

1. `bitrouter-registry` public GitHub repository。
2. JSON schema 与 validator scripts。
3. CI 校验 PR。
4. committed aggregate file `registry/v0/registry.json`。
5. `bitrouter` CLI 的本地 raw-file fetch / cache / verify 逻辑。

不做：

- 不部署 Supabase / Postgres / Next.js / Vercel 服务；
- 不提供 query API、publish API、admin API、OpenAPI；
- 不收取 registry mutation MPP fee；
- 不保存 Provider 私钥；
- 不代理 LLM 请求；
- 不实现 DHT discovery；
- 不把 active provider 状态写入链上；
- 不做 Provider 准入、不要求维护者同意经营资格、不做 KYC / curated set 判断。

---

## 1. Repository 形态

建议仓库仍命名为 `bitrouter-registry`，但它是静态数据仓库，不是应用服务：

```text
bitrouter-registry/
├── registry/
│   └── v0/
│       ├── registry.json                 # committed full registry artifact; raw URL 读取入口
│       ├── nodes/
│       │   └── <node_id>.json            # one public-visible node per file
│       ├── tombstones/
│       │   └── <node_id>-<seq>.json      # optional signed delete / retire proof
│       └── schemas/
│           ├── registry.schema.json
│           ├── node.schema.json
│           └── tombstone.schema.json
├── scripts/
│   ├── validate-registry.ts
│   └── build-registry.ts
├── docs/
│   ├── submitting-node.md
│   └── trust-model.md
├── .github/
│   ├── workflows/
│   │   └── validate.yml
│   └── pull_request_template.md
└── package.json
```

### 1.1 Source item 与 aggregate artifact

`registry/v0/nodes/*.json` 是人工 PR 修改的 source item。每个文件表示一个 public-visible Provider node。

`registry/v0/registry.json` 是完整 registry artifact，供 Consumer 通过 GitHub raw 一次性读取。它由 `scripts/build-registry.ts` 从 source item 生成，并随 PR 一起提交。CI 必须验证 committed artifact 与 source item 完全一致，避免 raw 读取入口落后于 source。

这种设计保留两种需求：

1. 对贡献者：小文件 PR，冲突少，审查清晰。
2. 对 Consumer：单个 raw JSON 文件即可获得完整 registry 数据，不需要 GitHub API，不需要服务端查询 API。

### 1.2 不使用 GitHub API 作为协议依赖

Consumer 默认只访问 raw file：

```text
GET https://raw.githubusercontent.com/bitrouter/bitrouter-registry/main/registry/v0/registry.json
```

不依赖 GitHub REST / GraphQL API，不需要 token，不需要搜索仓库文件列表。GitHub PR、Actions、review 是维护 registry 数据的协作机制，不是 P2P 协议运行时依赖。

---

## 2. Registry file item

### 2.1 Node item

Node item 是 Provider 对一个 public-visible node 的自签公告。文件名应由 `node_id` 派生：

```text
registry/v0/nodes/ed25519_z6mk....json
```

示例：

```jsonc
{
  "schema_version": "bitrouter.registry.node.v0",
  "node_id": "ed25519:<z-base32>",
  "provider_id": "ed25519:<z-base32>",
  "seq": 7,
  "status": "active",
  "valid_until": "2026-07-01T00:00:00Z",
  "display_name": "Example Provider - US East",
  "contact": {
    "website": "https://example.com",
    "security": "mailto:security@example.com"
  },
  "endpoints": [
    {
      "endpoint_id": "ed25519:<z-base32>",
      "status": "active",
      "region": "geo:us-east-1",
      "relay_urls": ["https://relay-us.bitrouter.ai/"],
      "direct_addrs": [],
      "capacity": {
        "concurrent_requests": 100
      },
      "api_surfaces": ["openai_chat_completions", "anthropic_messages"],
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
          "method_details": {
            "chain_id": 4217,
            "fee_payer": true
          },
          "rates": {
            "input": { "numerator": "3000000", "denominator": "1000000" },
            "output": { "numerator": "15000000", "denominator": "1000000" }
          }
        }
      ]
    }
  ],
  "accepted_pgws": {},
  "signature": {
    "algorithm": "ed25519",
    "key_id": "ed25519:<z-base32>",
    "value": "ed25519:<sig>"
  }
}
```

签名规则：

1. `signature.value` 覆盖去掉 `signature` 字段后的 canonical JSON。
2. `signature.key_id` 必须等于 `provider_id`，除非未来引入显式 delegation 文件。
3. `seq` 对同一个 `node_id` 单调递增。
4. `valid_until` 必须存在；过期 item 仍可保留在仓库中，但 Consumer 默认不使用。

### 2.2 Status

v0 item status 只表达节点自公告的运行状态：

| Status | 含义 |
|---|---|
| `active` | Consumer 默认可选择该 node |
| `draining` | 节点仍可处理已有流量，但新流量应降低优先级或避开 |
| `disabled` | 节点公开可见但不应被 Consumer 选择，常用于临时维护 |

彻底退出可以删除 node item。删除后 aggregate file 不再包含该 node。

### 2.3 Delete / shutdown proof

为了避免第三方伪造删除别人的 node，删除 PR 必须满足至少一个条件：

1. PR 同时新增 `registry/v0/tombstones/<node_id>-<seq>.json`，其中 tombstone 由 `provider_id` root key 签名；
2. PR 先把 node item 更新为 `status: "disabled"` 或较高 `seq` 的 retire form，merge 后再做清理删除；
3. 维护者能通过其他公开、可审计方式确认 Provider 授权删除。

推荐 tombstone 形式：

```jsonc
{
  "schema_version": "bitrouter.registry.tombstone.v0",
  "node_id": "ed25519:<z-base32>",
  "provider_id": "ed25519:<z-base32>",
  "seq": 8,
  "reason": "retired",
  "effective_at": "2026-04-28T00:00:00Z",
  "signature": {
    "algorithm": "ed25519",
    "key_id": "ed25519:<z-base32>",
    "value": "ed25519:<sig>"
  }
}
```

Tombstone 不进入 active registry；它只作为 Git history 中的可审计删除证明。

---

## 3. Full registry artifact

`registry/v0/registry.json` 是 Consumer 默认读取的完整文件：

```jsonc
{
  "schema_version": "bitrouter.registry.v0",
  "generated_at": "2026-04-28T00:00:00Z",
  "source": {
    "repository": "github.com/bitrouter/bitrouter-registry",
    "branch": "main",
    "commit": "<git-sha>"
  },
  "nodes": [
    {
      "schema_version": "bitrouter.registry.node.v0",
      "node_id": "ed25519:<z-base32>",
      "provider_id": "ed25519:<z-base32>",
      "seq": 7,
      "status": "active",
      "valid_until": "2026-07-01T00:00:00Z",
      "endpoints": [],
      "models": [],
      "accepted_pgws": {},
      "signature": {}
    }
  ]
}
```

Artifact 规则：

1. `nodes` 按 `node_id` 字典序稳定排序。
2. 不包含 tombstone。
3. 不包含 `status != active|draining|disabled` 的 item。
4. 不包含 schema invalid、signature invalid、expired-too-far、seq regression 的 item。
5. 可包含当前已过期 item，但 Consumer 默认必须过滤；是否在 build 阶段剔除可由仓库 policy 决定。
6. `source.commit` 应等于生成 artifact 的 commit，或在无法自引用时留空并由 release/tag manifest 补充。Consumer 不应把该字段作为唯一信任根。

---

## 4. PR mutation flow

### 4.1 Deploy new public-visible node

1. Provider 本地生成或读取 `provider_id` / `node_id`。
2. Provider 从本地 config 生成 node item。
3. Provider root key 对 canonical item 签名。
4. Provider 新增 `registry/v0/nodes/<node_id>.json`。
5. Provider 运行 `pnpm validate` 与 `pnpm build:registry`。
6. Provider 提交 PR。
7. CI 验证 schema、签名、seq、artifact consistency。
8. 维护者 merge 后，raw `registry.json` 对外可见。

### 4.2 Modify node

修改 endpoint、model、pricing、capacity、status、contact 等字段时：

1. 增加 `seq`。
2. 更新 `valid_until`。
3. 重新签名。
4. 重新生成 aggregate。
5. 提交 PR。

CI 必须比较 base branch 中同一 `node_id` 的旧 item，拒绝 `seq` 回退或重复。

### 4.3 Shutdown / delete node

临时维护优先改为：

```json
{ "status": "disabled" }
```

永久退出可以删除 source item。删除 PR 必须附带 tombstone 或其他可审计授权证明。merge 后 aggregate 不再包含该 node；Consumer 下次 sync 后自然停止选择该 node。

---

## 5. Validation policy

CI 与本地 validator 必检：

1. JSON schema。
2. canonical JSON signature。
3. `provider_id` / `node_id` / `endpoint_id` 格式。
4. `signature.key_id == provider_id`。
5. `seq` 对同一 `node_id` 单调递增。
6. `valid_until` 存在，且不超过仓库 policy 的最大未来窗口。
7. endpoint status、region、relay URL、direct addr syntax 合法。
8. endpoint count / model count / pricing count 不超过 v0 上限。
9. model 与 `api_surface` 合法。
10. pricing 金额字段为 base-unit integer string 或 rational。
11. MPP payment asset descriptor 合法：`currency` 不得单独作为跨网络资产身份；Tempo 必须同时校验 `method == "tempo"`、TIP-20 `currency`、`recipient` 地址格式、`method_details.chain_id` 与 token allowlist。
12. aggregate artifact 与 source item 重新生成结果一致。
13. 删除 PR 的 tombstone 或授权证明存在。

CI 不做：

- 不验证 Provider 是否有商业资质；
- 不验证 KYC；
- 不判断 Provider 是否属于 BitRouter curated set；
- 不做在线 endpoint health check 作为 merge 前置条件；
- 不要求 Provider 支付 mutation fee。

Endpoint health 可以作为非阻塞 report comment 或后续 reputation signal，但不能成为 v0 Registry 的协议准入条件。

---

## 6. Consumer 读取语义

Consumer registry client 只需要 raw-file fetch、cache、verify、local query：

```text
raw registry.json
  -> schema validate
  -> verify each node signature
  -> filter status == active
  -> filter valid_until > now
  -> filter model / api_surface / region / pricing / local trust policy
  -> dial endpoint
```

推荐缓存行为：

1. 使用 HTTP `ETag` / `Last-Modified` 做 conditional request。
2. 本地保存最近一次通过校验的 registry。
3. 新 raw 文件下载成功但校验失败时，不替换 last-known-good cache，并显式告警。
4. cache 过旧时降低 P2P 自动选择能力，但允许用户显式使用本地配置中的 Provider。
5. 支持配置 raw URL mirror，例如企业内部 mirror、GitHub release asset、IPFS snapshot；但默认入口仍是 GitHub raw。

Consumer 不需要：

- API key；
- GitHub token；
- Supabase key；
- Registry login；
- publish credential；
- mutation fee payment。

---

## 7. CLI 集成

主 `bitrouter` CLI 只需要围绕静态文件提供辅助命令：

```bash
bitrouter p2p identity show
bitrouter p2p registry item export
bitrouter p2p registry tombstone export
bitrouter p2p registry sync
bitrouter p2p registry verify
bitrouter p2p dial-test <provider_id-or-node_id>
bitrouter p2p status
```

CLI responsibilities：

1. 从本地配置生成 node item。
2. 使用 Provider root key 签名。
3. 输出可直接放入 `bitrouter-registry/registry/v0/nodes/` 的 JSON 文件。
4. 可选生成 tombstone JSON。
5. 下载 raw `registry.json`，验证并缓存。
6. 本地按 model / region / pricing / trust policy 查询。

CLI 不做：

- `registry login`；
- `registry publish` HTTP mutation；
- 自动支付 registry gas fee；
- 持有任何 Registry 服务端 credential。

如果未来提供 `bitrouter p2p registry pr create`，它只能是 GitHub PR 创建便利功能，本质仍是提交文件变更，不是协议 publish API。

---

## 8. Trust model

v0 Registry 的信任分层：

1. **Provider signature**：证明 item 内容由 `provider_id` root key 授权。
2. **GitHub repository history**：提供公开审查、回滚、审计、PR discussion 与变更时间线。
3. **CI validation**：防止格式错误、签名错误、seq regression、明显垃圾数据进入 main。
4. **Consumer local policy**：最终决定是否信任某 Provider / node / model / price。

重要边界：

- Registry merge 不是 BitRouter endorsement。
- Registry item 存在不代表 Provider 安全、可靠、合规或有库存。
- Consumer 必须把 registry 内容当作 provider-signed advertisement，而不是中心化权威 API 响应。
- GitHub raw availability 是 v0 的便利分发机制，不是长期去中心化可用性保证。

未来可以平滑迁移：

- 同一 node item 可发布到 DHT。
- aggregate 可作为 release asset、IPFS CID 或链上 commitment 的 payload。
- `seq` / `valid_until` / signature 模型可映射到未来链上状态机。

---

## 9. Anti-abuse 与维护者职责

由于 v0 不运行写入 API，主要反滥用手段来自 GitHub 协作流程：

- PR review；
- CI validation；
- CODEOWNERS；
- branch protection；
- file size / count limits；
- schema-level endpoint / model / pricing limits；
- GitHub spam controls；
- public audit trail。

维护者可以拒绝或 revert：

1. schema invalid 或 CI failed 的 PR；
2. 签名不匹配、seq 回退、delete 未授权的 PR；
3. 明显垃圾、恶意、钓鱼、违法或会伤害网络安全的内容；
4. 破坏 raw artifact、绕过 build、试图提交 secrets 的内容；
5. 违反仓库文档中客观格式与安全规则的内容。

维护者不应因为以下原因拒绝一个格式合法、签名合法、无明显滥用的 item：

- Provider 不是 BitRouter 客户；
- Provider 没有 KYC；
- Provider 不在 curated set；
- Provider 与 BitRouter 没有商业合同；
- Provider 的价格不是维护者偏好的价格。

---

## 10. Test / CI 设计

建议仓库使用轻量 TypeScript validator；不需要 app integration tests。

```jsonc
{
  "scripts": {
    "validate": "tsx scripts/validate-registry.ts",
    "build:registry": "tsx scripts/build-registry.ts",
    "check": "pnpm build:registry && pnpm validate && git diff --exit-code registry/v0/registry.json"
  }
}
```

CI 必须覆盖：

| 编号 | 场景 | 断言 |
|---|---|---|
| CI-1 | 新增合法 signed node item | validation pass，aggregate 包含该 node |
| CI-2 | schema invalid | validation fail |
| CI-3 | signature invalid | validation fail |
| CI-4 | `seq` 回退或重复 | validation fail |
| CI-5 | Tempo pricing 缺少 `method_details.chain_id` | validation fail |
| CI-6 | 只用裸 `currency` 表示 payment asset | validation fail |
| CI-7 | source item 改了但 aggregate 未更新 | validation fail |
| CI-8 | 删除 node 但无 tombstone / 授权证明 | validation fail |
| CI-9 | 过大的 endpoints / models 列表 | validation fail |
| CI-10 | raw `registry.json` 可被 `bitrouter-p2p` fixture 验证并本地查询 | validation pass |

---

## 11. 与链上 / DHT 的关系

v0 明确不使用：

- DHT discovery；
- on-chain active provider set；
- on-chain provider metadata；
- chain event indexing 作为 Consumer 查询来源。

但 v0 的文件格式要为迁移保留空间：

- 每个 node item 独立签名，可脱离 GitHub 验证；
- `seq` 支持状态递进；
- `valid_until` 支持本地过期策略；
- aggregate file 可被镜像、固定哈希、发布到 release / IPFS / future DHT；
- tombstone 可表达授权退出。

---

## 12. 验收标准

| 编号 | 标准 |
|---|---|
| REG-1 | `bitrouter-registry` 不需要部署任何 Supabase / Next.js / Vercel / API 服务 |
| REG-2 | Provider 通过 PR 新增合法 signed node item 后，merge 即公开出现在 raw `registry.json` |
| REG-3 | Provider 修改 node 必须提高 `seq` 并重新签名 |
| REG-4 | Provider shutdown 可通过 `status: disabled` 或带 tombstone 的删除 PR 完成 |
| REG-5 | Consumer 只读取 raw `registry.json`，不调用 query API |
| REG-6 | CLI 可生成 node item / tombstone，并可 sync / verify raw registry |
| REG-7 | CI 拒绝 invalid signature、stale seq、invalid pricing、artifact mismatch |
| REG-8 | Registry merge 不代表准入、KYC、商业背书或 curated set |
| REG-9 | Consumer 本地验证 signature、`valid_until`、status 后再选择 endpoint |
| REG-10 | Tempo payment asset 校验继续禁止用裸 `currency` 代表跨网络资产 |
