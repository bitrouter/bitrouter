# 007-06 — v3 网络原型实现验收简报

> 状态：**v0.1 — 验收简报**。本文根据 `~/code/bitrouter/bitrouter-p2p-proto` 当前实现、`docs/V3_IMPLEMENTATION_REPORT.md`、[`007-05`](./007-05-proto-prd-v3.md) 的 v3 PRD，以及本轮原型实现记录，对 v3 网络原型做一次成果总结。
>
> 原型仓库 HEAD：`c7f2597 Add v3 prototype implementation report`。

---

## 0. 结论

v3 网络原型可以判定为**完成了第三版 PRD 的核心验证目标**：在不重做 v2 网络拓扑的前提下，把 Registry、Direct path、Session Control path、Payment Receipt、Tempo voucher wrapper 与错误对象迁移到新的 Type ID、base58btc 编码、JCS signed envelope / proof profile 和 BitRouter error object。

本版原型验证的重点不是新增网络角色或生产功能，而是证明：**协议对象格式收敛以后，v2 已经跑通的 Direct / PGW / Tempo lifecycle 网络路径仍然可以端到端运行，并且旧 wire format 会被拒绝。**

已完成的主要成果：

- `bitrouter-core` 新增 v3 protocol primitives：Type ID、base58btc codec、canonical hash、signed envelope、Ed25519-JCS proof、BitRouter error object。
- Registry runtime 改为只加载 root `registry.json` aggregate，使用 signed node / tombstone envelope；旧 `providers/` / `pgws/` directory loading 被拒绝。
- Direct `Payment-Receipt` 改为 `bitrouter/payment/receipt/0` signed envelope；GET fallback 返回同一 envelope。
- Session Control voucher / epoch close 改为 root `proofs[]` + Ed25519-JCS proof；legacy inline `signature` 被拒绝。
- Tempo voucher 增加 `bitrouter/tempo/voucher/0` wrapper；EVM / MPP 边界保留原生 hex 与 typed data，BitRouter wrapper 内使用 base58btc signature。
- HTTP canonical error response 改为 `application/vnd.bitrouter.error+json` + `bitrouter/error/0`，不再把 URL-based top-level `type` 作为 P2P canonical wire。
- 原型仓库 vendored 了 Tempo channel creation bytecode fixture 与 provenance，使本地 Tempo open / lifecycle 脚本不再依赖外部编译产物。

---

## 1. 本次成果来源

### 1.1 读取文档

- [`007-05 — v0 网络原型第三版 PRD`](./007-05-proto-prd-v3.md)
- [`008-03 — bitrouter-registry：v0 网络公开静态注册表`](./008-03-bitrouter-registry.md)
- 原型仓库 `docs/V3_IMPLEMENTATION_REPORT.md`
- 原型仓库 `docs/PRD_V3.md`
- 原型仓库 `docs/p2p/001-03-protocol-conventions.md`
- 原型仓库 `docs/p2p/001-04-api-reference-examples.md`

### 1.2 原型仓库关键提交

| Commit | Summary |
|---|---|
| `4585971` | Add v3 protocol foundations |
| `5fed65b` | Migrate direct receipts to v3 envelopes |
| `79147b8` | Require v3 registry aggregates |
| `facbf65` | Migrate session payments to v3 proofs |
| `7a9b466` | Add session proof rejection tests |
| `5221bdf` | Vendor Tempo channel bytecode fixture |
| `c7f2597` | Add v3 prototype implementation report |

---

## 2. v3 PRD 对照验收

| PRD 主题 | 验收结论 | 证据 / 说明 |
|---|---|---|
| 拓扑不重做 | 通过 | 沿用 v2 Consumer / Provider / PGW / Registry / iroh relay / Tempo localnet 拓扑；本版只替换协议对象格式 |
| Type ID validation | 通过 | Type IDs 集中到 `bitrouter-core::protocol`；Direct ALPN 为 `bitrouter/direct/0`，Session Control ALPN 为 `bitrouter/session/control/0` |
| base58btc codec | 通过 | Ed25519 pubkey 使用 `ed25519:<base58btc>`；Ed25519 signature 使用裸 base58btc；SHA-256 digest 使用 `sha256:<base58btc>` |
| Signed envelope | 通过 | 新增 `{ type, payload, proofs[] }` envelope 与 `bitrouter/proof/ed25519-jcs/0` verifier；签名输入固定 `bitrouter-signature-input/0\n + JCS(...)` |
| BitRouter error object | 通过 | 新增 `bitrouter/error/0` 与 `application/vnd.bitrouter.error+json`；Direct receipt fallback 等 runtime path 使用 v3 error payload |
| Registry aggregate | 通过 | runtime loader 只接受 root `registry.json` / `bitrouter/registry/0` aggregate，校验 signed node / tombstone envelope |
| Registry legacy rejection | 通过 | 覆盖 `schema_version`、inline `sig`、wrong item type、wrong signer、missing aggregate、legacy directory loading 等负向路径 |
| Direct receipt envelope | 通过 | `Payment-Receipt` 迁移到 `bitrouter/payment/receipt/0` signed envelope；trailer 与 GET fallback 返回同一对象 |
| Tempo voucher wrapper | 通过 | 新增 `bitrouter/tempo/voucher/0` + `bitrouter/proof/eip712/0`；wrapper signature 为 base58btc，EVM 边界保留 hex |
| Session Control proofs | 通过 | PGW voucher / epoch close frame 迁移到 root `proofs[]`；inline legacy `signature` 被 deserialization / verification tests 拒绝 |
| Negative compatibility | 通过 | 增加 legacy encoding、inline signature、missing proof、legacy Registry fields、URL-based error shape drift 等拒绝测试 |
| v2 topology regression | 通过 | Direct / PGW path、receipt fallback、Tempo open / lifecycle 仍可运行 |

---

## 3. 关键实现成果

### 3.1 协议基础设施收敛

v3 把分散在不同路径里的命名、编码和签名规则收敛到共享模块：

| 能力 | v3 结果 |
|---|---|
| Type ID | 统一 `bitrouter/<namespace>/<name>/<major>`，例如 `bitrouter/registry/0`、`bitrouter/payment/receipt/0`、`bitrouter/session/payment-voucher/0` |
| ALPN | Direct 为 `bitrouter/direct/0`；Session Control 为 `bitrouter/session/control/0` |
| Opaque bytes | BitRouter-owned public key / signature / digest 使用 base58btc；EVM address、tx hash、EIP-712 signature 在外部边界保留标准格式 |
| Envelope | 所有 BitRouter signed object 采用 `{ type, payload, proofs[] }` |
| Proof | Ed25519-JCS 使用 detached proof；Tempo voucher wrapper 使用 EIP-712 proof profile |
| Error | P2P canonical wire 使用 `bitrouter/error/0`，文档 URL 只放在 `payload.doc_url` |

这一层的意义是：后续协议对象新增时，不再为每个对象单独发明 `schema_version`、`sig`、`signature`、URL-based `type` 或不同 digest encoding。

### 3.2 Registry 回到 public static file 模型

v3 原型把 Registry read path 简化为一个 root aggregate；该方向已经在 [`008-03`](./008-03-bitrouter-registry.md) 正式规范化为 v0 网络公开静态注册表：

```text
v0/registry.json
└── type: bitrouter/registry/0
    └── nodes[]: bitrouter/registry/node/0 signed envelopes
```

实现结果：

1. runtime loader 必须找到 root `registry.json`。
2. aggregate 必须是 `bitrouter/registry/0`。
3. active node 必须是 `bitrouter/registry/node/0` signed envelope。
4. tombstone 必须是 `bitrouter/registry/tombstone/0` signed envelope，并由被删除 node 的 root key 签名。
5. loader 会验证 proof signer 与 `provider_id` / `pgw_id` 的一致性。
6. 旧 `providers/` / `pgws/` directory loading 不再作为 runtime fallback。

这与 `008-03` 固化的 v0 registry 维护方式一致：v0 不运行任何 `bitrouter-registry` 服务；公共可见节点的创建、修改、删除通过 GitHub PR 改动 `nodes/` 与 `tombstones/` source item，并随 PR 提交生成后的 `/v0/registry.json`。Consumer 只读取 GitHub raw 上的 `/v0/registry.json` 后离线验签，不调用 Registry API，也不依赖 GitHub REST / GraphQL API。

### 3.3 Direct path 迁移

Direct path 保留 v2 的网络行为：Consumer 通过 Registry 找 Provider，建立 HTTP/3 over iroh Direct connection，按 MPP 402 / credential / receipt 流程完成支付与 SSE。

v3 改动集中在 BitRouter-owned extension：

| 对象 | v2 / 历史形态 | v3 形态 |
|---|---|---|
| Direct ALPN | 已使用 Direct 专用 ALPN | `bitrouter/direct/0` 常量化 |
| Tempo voucher | MPP / EVM 边界内的 native voucher | BitRouter 内部增加 `bitrouter/tempo/voucher/0` wrapper |
| EIP-712 signature | `0x...` hex | wallet / RPC / MPP 边界保持 hex；wrapper 内转 base58btc |
| Payment receipt | legacy inline signature record | `bitrouter/payment/receipt/0` signed envelope |
| Receipt fallback error | historical problem-like shape | `bitrouter/error/0` payload |

因此 Direct path 证明了一个重要边界原则：**外部标准保持原样，BitRouter 自有对象在 wrapper / envelope 边界收敛。**

### 3.4 Session Control 迁移

Session path 保留 v2 的 Data / Control split：

- Data Connection：标准 `h3`，业务请求只带 `BR-Order-Ref`。
- Control Connection：`bitrouter/session/control/0`，负责 voucher、stream completed、epoch close、payment error。

v3 的主要变化：

1. Control frame payload 使用 snake_case 与完整 Type ID。
2. `bitrouter/session/payment-voucher/0` 使用 PGW `pgw_id` 签名的 Ed25519-JCS proof。
3. `bitrouter/session/payment-epoch-close/0` 使用同样 proof 机制。
4. payload 内不再允许 inline `signature`。
5. `payment-error` payload 与 `bitrouter/error/0.payload` 对齐，而不是 RFC 9457 problem object。

这说明 v3 格式迁移没有破坏 Session path 的核心业务语义：nonce、cumulative amount、collateral monotonicity、PGW restart recovery 等 v2 逻辑继续保留。

### 3.5 Tempo localnet fixture 可复现性

v2 验收时，完整 Tempo lifecycle 依赖外部 `BR_TEMPO_ESCROW_BYTECODE`、`BR_TEMPO_DEPLOYER_PRIVATE_KEY`、`BR_TEMPO_DEPLOYER_ADDRESS`。v3 原型补齐了一个更可复现的默认路径：

- `tests/fixtures/tempo/TempoStreamChannel.bytecode.txt`
- `tests/fixtures/tempo/TempoStreamChannel.provenance.json`

当前官方 `tempoxyz/tempo` `main` 分支提供 specs、verification harness、`ITempoStreamChannel` references，但没有在旧路径暴露可直接部署的 `TempoStreamChannel.sol` implementation。因此原型暂时 vendored 一个编译后的 creation bytecode fixture，并记录 source mirror、commit、compiler settings 与 artifact hash。

这不是生产依赖决策；正式版本仍应在 Tempo 官方发布 deployable implementation 后替换 fixture。

---

## 4. 验证记录

原型仓库报告记录以下验证通过：

```bash
cargo test --workspace --quiet
cargo test -p bitrouter-core --quiet
cargo test -p bitrouter-registry --quiet
cargo test -p bitrouter-mpp-adapter --quiet
cargo test -p bitrouter-node --test direct_path --quiet
cargo test -p bitrouter-node --test pgw_path --quiet
BR_DIRECT_TOTAL_N=1 BR_DIRECT_CONCURRENCY=1 examples/direct-e2e-real.sh
BR_PGW_TOTAL_N=2 BR_PGW_CONCURRENCY=2 examples/pgw-e2e-real.sh
examples/direct-e2e-tempo-open.sh
examples/direct-e2e-tempo-session-lifecycle.sh
```

最终 Tempo lifecycle run 使用 checked-in fixture defaults，不再要求额外配置 `BR_TEMPO_ESCROW_BYTECODE`、`BR_TEMPO_DEPLOYER_PRIVATE_KEY` 或 `BR_TEMPO_DEPLOYER_ADDRESS`。流程覆盖：

1. 部署本地 escrow。
2. open session channel。
3. 读取链上 escrow state 并校验。
4. 发送第二个 cumulative voucher request。
5. close / settle channel。
6. 观察最终链上 `finalized=true` 与 settlement transaction hash。

---

## 5. 本版原型证明了什么

### 5.1 协议格式可以独立于网络拓扑演进

v3 没有重写 Direct / PGW / Tempo lifecycle 拓扑，只替换了 Registry、receipt、voucher、control frame、error 等对象格式。这证明协议对象的规范收敛可以作为独立迭代推进，不必和网络层、支付层、拓扑层重构绑定。

### 5.2 Public Registry 已固定为静态文件规范

一个 committed `/v0/registry.json` + signed node / tombstone envelope 已足够支持 v0 public visible nodes 的发布、修改、删除和离线验证。`008-03` 已把维护流程规范为 public GitHub repository：运营方通过 PR 修改 `nodes/` / `tombstones/`，CI 运行 TypeScript/Zod 校验、签名校验、tombstone 校验、pricing / endpoint 静态校验，并要求重新生成的 `/v0/registry.json` 无 diff。

### 5.3 Wrapper boundary 是处理外部协议的正确位置

MPP、EIP-712、EVM JSON-RPC 这些外部协议不应该被 BitRouter 的 base58btc / envelope 规则强行改写。v3 的做法是：

- 外部边界保留 native shape。
- 进入 BitRouter-owned object 后使用 wrapper / proof profile。
- runtime 明确验证转换点。

这降低了与 MPP / Tempo SDK 的互操作风险，也避免 BitRouter 内部继续产生多套签名格式。

### 5.4 Legacy rejection 应成为 CI blocker

v3 不追求兼容旧原型 wire format。旧格式只能保留在历史文档或 `tests/fixtures/legacy`，不能被 runtime 主路径接受。负向兼容测试在本版里不只是补充测试，而是协议迁移是否完成的核心证据。

---

## 6. 仍需注意的后续项

| 后续项 | 说明 |
|---|---|
| 官方 Tempo channel artifact | 当前 bytecode fixture 来自带 provenance 的临时 source mirror。Tempo 官方发布 deployable implementation 后，应替换为官方 source 编译产物 |
| Registry public repo 落地 | `008-03` 已规范 public GitHub repository、`nodes/` / `tombstones/`、`/v0/registry.json`、`generate.ts` / `validate.ts` / `manage.ts` 与 CI 校验模型；后续是创建正式 `bitrouter-registry` 仓库并实现这些脚本 / workflow |
| Cross-language golden vectors | 当前原型侧已有 Rust tests / fixtures；若要作为长期协议规范，应把 Type ID、JCS hash、proof、receipt、registry item、error object 的 golden vectors 固化到 `001-04` 或独立 fixtures |
| Gateway error projection | P2P canonical wire 已固定 `bitrouter/error/0`；若外部 HTTP gateway 需要 RFC 9457 `application/problem+json`，应作为 gateway projection，不回流到 P2P wire |
| Registry distribution hardening | `008-03` 已明确 v0 默认读取 GitHub raw `/v0/registry.json`；生产阶段仍需细化 mirror、CDN cache、commit hash pinning、rollback 与 emergency tombstone 操作流程 |

---

## 7. 验收结论

v3 网络原型已把 v2 中“跑通真实网络拓扑”的成果推进到“协议对象格式收敛”的阶段：

1. Registry 从 legacy directory / snapshot 迁移到 static `registry.json` aggregate。
2. BitRouter-owned JSON 对象统一 Type ID、snake_case、base58btc、signed envelope / proof profile。
3. Direct receipt、Session voucher / epoch close、Tempo voucher wrapper 和 error object 均迁移到 v3 格式。
4. 旧 wire format 在 runtime 主路径中被明确拒绝。
5. v2 已通过的 Direct / PGW / Tempo lifecycle 路径在格式替换后仍可运行。

因此，本简报建议将 v3 prototype 标记为：**第三版 PRD 核心目标验收通过；下一阶段应按 `008-03` 落地正式 `bitrouter-registry` public repo，并把跨语言 golden vectors、官方 Tempo channel artifact 替换方案继续沉淀为正式协议 / 工程任务。**
