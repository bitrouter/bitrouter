# 008-03 — `bitrouter-registry`：v0 网络公开静态注册表

> 状态：**v0.4 — Public repository / generated static registry 设计**。
>
> 本文定义 v0 P2P 网络的公开 Registry 形态：一个公开 GitHub 仓库、一组可审查的 provider-owned source item、一个由仓库脚本生成并提交的完整 registry 文件。它不是动态服务、不是数据库、不是 API、不是 DHT，也不是链上 Registry。正式网络部署见 [`008-01`](./008-01-network-deployment.md)；主仓库集成见 [`008-02`](./008-02-main-repo-integration.md)。

---

## 0. 结论

v0 不运行任何 `bitrouter-registry` 服务。Registry 只是一个公开数据仓库：

- Provider / node 运营方通过 PR 创建、修改或 tombstone 自己的 node item；
- 仓库 CI 只运行 TypeScript/Zod 校验、签名校验、tombstone 校验、pricing、endpoint 等静态校验；
- 维护者 merge PR 只代表“该变更符合公开仓库格式与反垃圾规则”，不代表准入、背书、KYC、商业合同或 curated provider set；
- Consumer 不调用 Registry API，只读取 GitHub raw 上 committed `/v0/registry.json` 静态文件，并在本地过滤 model、region、pricing、endpoint；
- Provider 不调用 publish API，不支付 registry mutation gas fee，不需要 Registry 账号；
- 所有可验证状态仍由 Provider root key 签名，GitHub 只是 v0 的公开分发与审查通道。

推荐的公开读取入口是一个普通静态 JSON URL，路径固定以 `/v0/registry.json` 结尾：

```text
https://raw.githubusercontent.com/bitrouter/bitrouter-registry/main/v0/registry.json
```

实现上只需要：

1. `bitrouter-registry` public GitHub repository。
2. `nodes/` 与 `tombstones/` 两个 source data 目录。
3. 根目录 TypeScript scripts：`generate.ts`、`validate.ts`、`manage.ts` 与 `shared/*`。
4. PR 同时提交重新生成的 `/v0/registry.json`；CI 再次运行 `generate.ts` 并要求工作树无 diff。
5. `bitrouter` CLI 的本地 static-file fetch / cache / verify 逻辑。

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
├── nodes/
│   └── <node_id>.json                    # one public-visible node per file
├── tombstones/
│   └── <node_id>-<seq>.json              # signed delete / retire proof
├── v0/
│   └── registry.json                     # generated full registry artifact; committed for GitHub raw
├── generate.ts                           # canonical validate + verify + generate pipeline
├── validate.ts                           # check-mode wrapper for CI / local validation
├── manage.ts                             # node maintainer helper: add / update / tombstone
├── shared/
│   └── registry-lib.ts                   # Zod schemas, JCS/proof, path and policy helpers
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

`nodes/*.json` 是人工 PR 修改的 source item。每个文件表示一个 public-visible Provider node。

`tombstones/*.json` 是 provider-signed retire/delete proof。它保留在 git history 与 main branch 中，供 CI 和审查者确认 node 删除不是第三方伪造。

`v0/registry.json` 是完整 registry artifact，供 Consumer 通过 GitHub raw 一次性读取。它由 `generate.ts` 从 `nodes/` 与 `tombstones/` 生成，并必须随 PR 一起提交。`/v0/` 是 tracked generated output：不手写、不加入 `.gitignore`，但作为仓库内容存在，确保 raw content URL 可直接访问。

这种设计保留两种需求：

1. 对贡献者：小文件 PR，冲突少，审查清晰；`manage.ts` 可把签名后的文件放到正确目录。
2. 对 Consumer：单个静态 JSON 文件即可获得完整 registry 数据，不需要 GitHub API，不需要服务端查询 API。
3. 对维护者：没有 JSON Schema 文件；所有结构校验、验签、tombstone 规则和生成逻辑都收敛在 TypeScript/Zod 管线中，并由 CI 防止 committed artifact drift。

### 1.2 不使用 GitHub API 作为协议依赖

Consumer 默认只访问静态 registry file：

```text
GET https://raw.githubusercontent.com/bitrouter/bitrouter-registry/main/v0/registry.json
```

不依赖 GitHub REST / GraphQL API，不需要 token，不需要搜索仓库文件列表。GitHub PR、Actions、review 是维护 registry 数据的协作机制，不是 P2P 协议运行时依赖。

默认分发介质是 GitHub raw content；GitHub Pages、release artifact、对象存储、CDN 或未来 mirror 只能作为可配置 mirror。协议只要求最终公开路径提供 `/v0/registry.json` 的普通 JSON 内容；`bitrouter-registry` main branch 中的 `/v0/registry.json` 必须是 committed generated artifact。

---

## 2. Registry file item

### 2.1 Node item

Node item 是 Provider 对一个 public-visible node 的自签公告。文件名应由 `node_id` 派生：

```text
nodes/ed25519_7F5k....json
```

示例：

```jsonc
{
  "type": "bitrouter/registry/node/0",
  "payload": {
    "node_id": "ed25519:<base58btc>",
    "provider_id": "ed25519:<base58btc>",
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
        "endpoint_id": "ed25519:<base58btc>",
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
    "accepted_pgws": {}
  },
  "proofs": [
    {
      "protected": {
        "type": "bitrouter/proof/ed25519-jcs/0",
        "payload_type": "bitrouter/registry/node/0",
        "signer": "ed25519:<base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

签名规则：

1. Node item 必须符合 [`001-03`](./001-03-protocol-conventions.md) 的 `{type, payload, proofs[]}` signed envelope。
2. `proofs[].protected.type` 必须是 `bitrouter/proof/ed25519-jcs/0`。
3. `proofs[].protected.signer` 必须等于 `payload.provider_id`，除非未来引入显式 delegation 文件。
4. `proofs[].protected.payload_hash` 必须等于 `sha256(JCS(payload))`，digest bytes 用 base58btc。
5. `payload.seq` 对同一个 `payload.node_id` 单调递增。
6. `payload.valid_until` 必须存在；过期 item 仍可保留在仓库中，但 Consumer 默认不使用。

### 2.2 Status

v0 item status 只表达节点自公告的运行状态：

| Status | 含义 |
|---|---|
| `active` | Consumer 默认可选择该 node |
| `draining` | 节点仍可处理已有流量，但新流量应降低优先级或避开 |
| `disabled` | 节点公开可见但不应被 Consumer 选择，常用于临时维护 |

彻底退出应提交 signed tombstone，并删除对应 `nodes/<node_id>.json`。生成后的 `/v0/registry.json` 不再包含该 node。

### 2.3 Delete / shutdown proof

为了避免第三方伪造删除别人的 node，删除 PR 必须同时新增 signed tombstone：

1. tombstone 路径为 `tombstones/<node_id>-<seq>.json`，其中 `<node_id>` 使用与 `nodes/` 相同的 safe filename 派生规则；
2. tombstone 由 `provider_id` root key 签名，`proofs[].protected.signer == payload.provider_id`；
3. `payload.seq` 必须大于被删除 node item 在 base branch 中的 `seq`；
4. tombstone merge 后，同一 `node_id` 不得继续存在于 `nodes/`。

推荐 tombstone 形式：

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
        "signer": "ed25519:<base58btc>",
        "payload_hash": "sha256:<base58btc>"
      },
      "signature": "<base58btc>"
    }
  ]
}
```

Tombstone 不进入 generated registry；它只作为 main branch 与 Git history 中的可审计删除证明。`generate.ts` / `validate.ts` 必须拒绝 tombstoned `node_id` 再次出现在 `nodes/` 中；如未来允许 node 复活，必须引入显式新 type 或 delegation/rotation 规则，而不是复用同一 tombstone 语义。

---

## 3. Full registry artifact

`v0/registry.json` 是 `generate.ts` 生成并提交到仓库的完整文件：

```jsonc
{
  "type": "bitrouter/registry/0",
  "updated_at": "2026-04-28T00:00:00Z",
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

Artifact 规则：

1. `nodes` 按 `node_id` 字典序稳定排序。
2. 不包含 tombstone。
3. 不包含 `status != active|draining|disabled` 的 item。
4. 不包含 Zod invalid、proof invalid、tombstoned、expired-too-far、seq regression 的 item。
5. 可包含当前已过期 item，但 Consumer 默认必须过滤；是否在 build 阶段剔除可由仓库 policy 决定。
6. `updated_at` 是 registry 内容更新时间，不是每次 generate 的构建时间；`generate.ts` 不应在合法 committed registry 上无条件覆盖该值。
7. `source.commit` 可以记录生成 artifact 所基于的 source commit；如果 PR 阶段无法可靠自引用当前 commit，可以留空或记录 base/head metadata。Consumer 不应把该字段作为唯一信任根。

---

## 4. PR mutation flow

### 4.1 Deploy new public-visible node

1. Provider 本地生成或读取 `provider_id` / `node_id`。
2. Provider 从本地 config 生成 node item。
3. Provider root key 按 [`001-03`](./001-03-protocol-conventions.md) 生成 `proofs[]`。
4. Provider 使用 `bun run manage add ...` 或手动新增 `nodes/<node_id>.json`。
5. Provider 运行 `bun run generate` 更新 committed `v0/registry.json`，再运行 `bun run validate`。
6. Provider 提交 PR。
7. CI 验证 Zod schema、签名、seq、tombstone、pricing、endpoint 等规则。
8. CI 再次运行 `generate.ts` 并要求 `git diff --exit-code` 为空；维护者 merge 后，GitHub raw 的 `/v0/registry.json` 即对外可见。

### 4.2 Modify node

修改 endpoint、model、pricing、capacity、status、contact 等字段时：

1. 增加 `seq`。
2. 更新 `valid_until`。
3. 重新签名。
4. 运行 `bun run manage update ...` 或手动替换 `nodes/<node_id>.json`。
5. 提交 PR。

CI 必须比较 base branch 中同一 `node_id` 的旧 item，拒绝 `seq` 回退或重复。

### 4.3 Shutdown / delete node

临时维护优先改为：

```json
{ "status": "disabled" }
```

永久退出可以删除 source item。删除 PR 必须附带 `tombstones/<node_id>-<seq>.json`。merge 后 generated registry 不再包含该 node；Consumer 下次 sync 后自然停止选择该 node。

---

## 5. Validation policy

CI 与本地 validator 必检：

1. `shared/*` 中的 Zod schema。
2. signed envelope 与 ed25519-JCS proof。
3. `provider_id` / `node_id` / `endpoint_id` 格式。
4. `proofs[].protected.signer == payload.provider_id`。
5. `seq` 对同一 `node_id` 单调递增。
6. `valid_until` 存在，且不超过仓库 policy 的最大未来窗口。
7. endpoint status、region、relay URL、direct addr syntax 合法。
8. endpoint count / model count / pricing count 不超过 v0 上限。
9. model 与 `api_surface` 合法。
10. pricing 金额字段为 base-unit integer string 或 rational。
11. MPP payment asset descriptor 合法：`currency` 不得单独作为跨网络资产身份；Tempo 必须同时校验 `method == "tempo"`、TIP-20 `currency`、`recipient` 地址格式、`method_details.chain_id` 与 token allowlist。
12. tombstone 文件名、payload、签名、`seq` 与当前 / base branch 中被删除 node 的关系合法。
13. `generate.ts` 输出 deterministic `/v0/registry.json`，保留已有 `updated_at`，且 CI 运行后不得改变仓库中的任何文件。
14. 删除 PR 的 tombstone 存在。

不再维护任何 `schemas/*.json`。JSON 的权威机器校验来自 TypeScript/Zod；人读语义由本文档与 `docs/` 说明。Zod schema 必须和签名、tombstone、pricing 等跨字段规则在同一条 validation pipeline 中执行，避免“schema 通过但业务规则失败”的两套系统漂移。

CI 不做：

- 不验证 Provider 是否有商业资质；
- 不验证 KYC；
- 不判断 Provider 是否属于 BitRouter curated set；
- 不做在线 endpoint health check 作为 merge 前置条件；
- 不要求 Provider 支付 mutation fee。

Endpoint health 可以作为非阻塞 report comment 或后续 reputation signal，但不能成为 v0 Registry 的协议准入条件。

---

## 6. Consumer 读取语义

Consumer registry client 只需要 static-file fetch、cache、verify、local query：

```text
/v0/registry.json
  -> Zod / structural validate
  -> verify each node proof
  -> filter status == active
  -> filter valid_until > now
  -> filter model / api_surface / region / pricing / local trust policy
  -> dial endpoint
```

推荐缓存行为：

1. 使用 HTTP `ETag` / `Last-Modified` 做 conditional request。
2. 本地保存最近一次通过校验的 registry。
3. 新 registry 文件下载成功但校验失败时，不替换 last-known-good cache，并显式告警。
4. cache 过旧时降低 P2P 自动选择能力，但允许用户显式使用本地配置中的 Provider。
5. 支持配置 registry URL mirror，例如企业内部 mirror、GitHub release asset、IPFS snapshot；但默认入口仍是 GitHub raw 上 committed `/v0/registry.json`。

Consumer 不需要：

- API key；
- GitHub token；
- Supabase key；
- Registry login；
- publish credential；
- mutation fee payment。

---

## 7. Registry scripts 与 CLI 集成

### 7.1 `bitrouter-registry` 根目录 scripts

`bitrouter-registry` 仓库只保留根目录 TypeScript scripts，不再维护 `scripts/` 子目录与 `schemas/` 子目录：

```bash
bun run generate
bun run validate
bun run manage keygen --out .keys/dev-provider.json
bun run manage add --config <provider-config> --key <provider-root-key>
bun run manage update --node <node_id> --config <provider-config> --key <provider-root-key>
bun run manage tombstone --node <node_id> --key <provider-root-key> --reason retired
```

职责划分：

1. `generate.ts` 是唯一 canonical pipeline：读取 `nodes/` 与 `tombstones/`，执行 Zod 校验、ed25519-JCS 验签、`seq` / tombstone / pricing / endpoint policy 校验，最后稳定排序并写出 `v0/registry.json`。
2. `validate.ts` 是 check-mode 入口：CI 与贡献者本地运行它来复用同一套 shared validator；它不得定义第二套 schema 或第二套验签逻辑。
3. `manage.ts keygen` 可生成简易 Ed25519 development key file，便于本地演示签名流程；它不是生产级密钥管理方案，输出目录应类似 `.keys/` 并被 git ignore。
4. `manage.ts add` 读取本地 provider config 与 root key，生成 signed node item，并写入 `nodes/<node_id>.json`。
5. `manage.ts update` 读取现有 `nodes/<node_id>.json`，提高 `seq`，按新 config 重新签名并覆盖同一路径。
6. `manage.ts tombstone` 生成 signed tombstone，写入 `tombstones/<node_id>-<seq>.json`，并删除或提示删除对应 `nodes/<node_id>.json`。
7. `shared/registry-lib.ts`（或同级 shared helper）承载 Zod schemas、JCS canonicalization、hash/sign/verify、safe filename、policy limit、Tempo pricing allowlist 等共享逻辑。

`manage.ts` 只在本地读取 provider root key；不得把私钥写入 `nodes/`、`tombstones/`、`v0/` 或任何 git-tracked config。

### 7.2 主 `bitrouter` CLI

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
3. 输出可直接放入 `bitrouter-registry/nodes/` 的 JSON 文件，或提示用户使用 registry repo 的 `manage.ts`。
4. 可选生成 tombstone JSON。
5. 下载 `/v0/registry.json`，验证并缓存。
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

1. **Provider proof**：证明 item 内容由 `provider_id` root key 授权。
2. **GitHub repository history**：提供公开审查、回滚、审计、PR discussion 与变更时间线。
3. **CI validation**：防止格式错误、签名错误、seq regression、明显垃圾数据进入 main。
4. **Consumer local policy**：最终决定是否信任某 Provider / node / model / price。

重要边界：

- Registry merge 不是 BitRouter endorsement。
- Registry item 存在不代表 Provider 安全、可靠、合规或有库存。
- Consumer 必须把 registry 内容当作 provider-signed advertisement，而不是中心化权威 API 响应。
- GitHub raw content availability 是 v0 的便利分发机制，不是长期去中心化可用性保证。

未来可以平滑迁移：

- 同一 node item 可发布到 DHT。
- generated registry 可作为 release asset、IPFS CID 或链上 commitment 的 payload。
- `seq` / `valid_until` / proof 模型可映射到未来链上状态机。

---

## 9. Anti-abuse 与维护者职责

由于 v0 不运行写入 API，主要反滥用手段来自 GitHub 协作流程：

- PR review；
- CI validation；
- CODEOWNERS；
- branch protection；
- file size / count limits；
- Zod / policy-level endpoint / model / pricing limits；
- GitHub spam controls；
- public audit trail。

维护者可以拒绝或 revert：

1. Zod invalid 或 CI failed 的 PR；
2. 签名不匹配、seq 回退、delete 未授权的 PR；
3. 明显垃圾、恶意、钓鱼、违法或会伤害网络安全的内容；
4. 破坏 source item、绕过 validation / generation、试图提交 secrets 的内容；
5. 违反仓库文档中客观格式与安全规则的内容。

维护者不应因为以下原因拒绝一个格式合法、签名合法、无明显滥用的 item：

- Provider 不是 BitRouter 客户；
- Provider 没有 KYC；
- Provider 不在 curated set；
- Provider 与 BitRouter 没有商业合同；
- Provider 的价格不是维护者偏好的价格。

---

## 10. Test / CI 设计

建议仓库使用轻量 TypeScript + Zod validator；不需要 app integration tests。

`bitrouter-registry` 默认使用 Bun 作为包管理器与 TypeScript script runner。

```jsonc
{
  "scripts": {
    "generate": "bun run generate.ts",
    "validate": "bun run validate.ts",
    "manage": "bun run manage.ts",
    "check": "bun run validate && bun run generate && git diff --exit-code"
  }
}
```

PR CI 运行 `bun install --frozen-lockfile`，然后运行 `bun run validate`，再运行 `bun run generate`，最后执行 `git diff --exit-code`。如果贡献者忘记提交更新后的 `v0/registry.json`，或 `generate.ts` 有非确定性输出，CI 必须失败。

CI 必须覆盖：

| 编号 | 场景 | 断言 |
|---|---|---|
| CI-1 | 新增合法 signed node item | validation pass，generated registry 包含该 node |
| CI-2 | Zod invalid | validation fail |
| CI-3 | proof invalid | validation fail |
| CI-4 | `seq` 回退或重复 | validation fail |
| CI-5 | Tempo pricing 缺少 `method_details.chain_id` | validation fail |
| CI-6 | 只用裸 `currency` 表示 payment asset | validation fail |
| CI-7 | `generate.ts` 后工作树出现 diff | validation fail |
| CI-8 | 删除 node 但无 tombstone | validation fail |
| CI-9 | 过大的 endpoints / models 列表 | validation fail |
| CI-10 | committed `/v0/registry.json` 可被 `bitrouter-p2p` fixture 验证并本地查询 | validation pass |

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
- generated registry file 可被镜像、固定哈希、发布到 release / IPFS / future DHT；
- tombstone 可表达授权退出。

---

## 12. 验收标准

| 编号 | 标准 |
|---|---|
| REG-1 | `bitrouter-registry` 不需要部署任何 Supabase / Next.js / API 服务；如使用 Pages / CDN，也只是静态文件发布 |
| REG-2 | Provider 通过 PR 新增合法 signed node item 并提交 regenerated `/v0/registry.json`，merge 后通过 GitHub raw 公开 |
| REG-3 | Provider 修改 node 必须提高 `seq` 并重新签名 |
| REG-4 | Provider shutdown 可通过 `status: disabled` 或带 tombstone 的删除 PR 完成 |
| REG-5 | Consumer 只读取 `/v0/registry.json`，不调用 query API |
| REG-6 | `manage.ts` 可执行 add / update / tombstone 并把文件写入 `nodes/` / `tombstones/`；CLI 可 sync / verify registry |
| REG-7 | CI 拒绝 Zod invalid、invalid proof、stale seq、invalid pricing、invalid tombstone、以及 `generate.ts` 后出现任何 git diff |
| REG-8 | Registry merge 不代表准入、KYC、商业背书或 curated set |
| REG-9 | Consumer 本地验证 proof、`valid_until`、status 后再选择 endpoint |
| REG-10 | Tempo payment asset 校验继续禁止用裸 `currency` 代表跨网络资产 |
