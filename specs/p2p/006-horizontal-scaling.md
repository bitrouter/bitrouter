# 006 — 横向扩容：Provider / Relay / PGW 多区域部署

> 状态：**v0.2 — 草案**。规定 BitRouter 协议中三类核心角色（Provider 节点、relay 服务、PGW）在跨区域横向扩容下的协议级约束与设计模式。重点回答：**MPP `session` channel 状态在多实例下如何保持一致性**。
>
> 上下文：[`002`](./002-l1-l2-mvp.md) §4 后续工作中提到"跨真机实验矩阵"暴露了单 home_relay 的延迟瓶颈；本文给出协议级横向扩容方案。
>
> 范围：
> - ✅ 协议层约束（Registry schema、channel 状态语义、密钥派生模式）。
> - ❌ 不展开成熟中心化运维方案（k8s 多副本、负载均衡、多区数据库、CDN 等）——业界已有标准做法，按各自 PGW operator 偏好选用。
>
> **变更历史**：
> - **v0.2**：与 [`003 v0.5`](./003-l3-design.md) 两层身份模型对齐——废弃 `logical_provider_id` / `logical_pgw_id` / `logical_id` 命名，统一使用 `provider_id` / `pgw_id`（ed25519 root pubkey）；Registry schema 从"多 endpoint 条目共享 logical_id"转为"一份 snapshot 内 `endpoints[]` 数组"；CI 一致性约束被 snapshot 原子签名天然保证（无需额外规则）。横向扩容机制本身不变。
> - **v0.1**：初稿。

---

## 0. TL;DR

- ==**核心结论**==：BitRouter 协议层不引入"逻辑 NodeId 抽象"；横向扩容统一走"==**多独立 `endpoint_id` + Registry `provider_id` / `pgw_id` 聚合**=="模式（[`002` 调研结论](./002-l1-l2-mvp.md) §4）——iroh 0.98 限制：单密钥不能跨多 relay 注册（`iroh-relay/src/server/clients.rs`：second-write 踢前者），单 NodeId pkarr 只能公告一个 home_relay。
- ==**两层身份**（与 [`003 §2.1`](./003-l3-design.md) 对齐）==：`provider_id` / `pgw_id`（ed25519 root pubkey，"逻辑实体"，签整份 snapshot）⊕ `endpoint_id`（iroh `EndpointId`，每进程一份）。横向扩容 = 同一份 snapshot 内的 `endpoints[]` 数组多于一项。
- **三类角色横向扩容机制完全不同**：
  - **Provider**：每地域独立 `endpoint_id` + 独立 chain 子密钥；同一 `provider_id` 的 snapshot 内 `endpoints[]` 列出全部实例。
  - **Relay**：iroh 原生 `RelayMap` 多 URL，与 NodeId 完全解耦；不需要任何 ID 聚合。
  - **PGW**：每地域独立 `endpoint_id` + ==**chain 子密钥**==；同一 `pgw_id` 的 snapshot 内 `endpoints[]` 列出全部实例；其余（Consumer 账户、应付账款）走传统中心化共享 DB（不在本协议范围）。
- ==**Session channel 状态的关键决策**==：**==每个 (PGW endpoint, Provider endpoint) pair 各自一条独立 channel==**——本协议**不引入跨实例 channel 状态同步**。voucher nonce / cumulative_amount 永远是单实例本地状态，零协调成本。代价是 N×M cartesian channel 数膨胀，由 Local Router 区域亲和路由 + 多余 channel idle close 缓解。
- 备选（仅当 N×M 真的扩到运维不可接受时启用）：**Sticky Leasing**——同一逻辑 channel 在多实例间通过分布式租约切单一 owner，owner 持有 nonce 状态；切主时新 owner 必须从共享存储读最新 voucher state 才能签新 voucher。本协议层为此预留扩展槽（§4.4），不内置实现。

---

## 1. 路径选型回顾

[`002` §4 后续工作 + 本次 iroh 0.98 调研](./002-l1-l2-mvp.md) 给出三条候选：

| 路径 | 描述 | 现实性 | 本文采用？ |
|---|---|---|---|
| A | 多独立 `endpoint_id` + Registry `provider_id` / `pgw_id` 聚合 | ✅ iroh 零修改 | ✅ **默认** |
| B | 单 `endpoint_id` 多 home_relay + pkarr 公告 `Vec<RelayUrl>` | ❌ 需上游 iroh + iroh-relay 大改（取消"单 NodeId 单 active client"约束） | ❌ 暂不规划 |
| C | L3 自定 logical pubkey + 子密钥派生（路径 A 的协议化版本） | ⚠️ 重复造身份层（003 §2.1 两层身份模型已经覆盖此功能） | ❌ 不再需要 |

==**本文规范化路径 A**==。==**与 003 v0.5 的两层身份模型完全等价**==——这里的 "logical 聚合" 在 003 中即 `provider_id` / `pgw_id`（root pubkey）；不再需要"路径 C"。

---

## 2. Provider 节点横向扩容

### 2.1 模型

```
                  ┌─────────────────────────────────────────┐
                  │   provider_id = ed25519:<root pubkey>    │
                  │      （"openrouter-eu" 是运维备注名）     │
                  └────┬─────────────┬─────────────┬─────────┘
                       │             │             │
                  endpoint_id_1  endpoint_id_2  endpoint_id_3
                  region=eu-w-1  region=eu-c-1  region=us-e-1
                  chain_addr_1   chain_addr_2   chain_addr_3
                  home_relay=R_eu1  R_eu2       R_us1
                  ───模型权重 + pricing + accepted_pgws 共享───
                  ───整个 snapshot 由 provider_id root key 一次签名───
```

### 2.2 必须共享 vs. 必须独立

| 维度 | 共享 | 独立 |
|---|---|---|
| 模型权重 / 上游 API key | 共享（运维同步） | — |
| `pricing[]` / `accepted_pgws` 配置 | 共享（同一 snapshot 字段） | — |
| `provider_id`（ed25519 root pubkey） | 共享（即"是谁"） | — |
| `endpoint_id`（ed25519，iroh `EndpointId`） | — | 每实例独立 |
| chain_addr（链上收款地址） | ==建议每实例独立子密钥==（详见 §4） | — |
| home_relay | — | 每实例就近 |
| MPP session channel | — | 每 (PGW endpoint, Provider endpoint) pair 各一条（§4.3） |

### 2.3 Registry schema 影响

[`003 §2.2`](./003-l3-design.md) Provider snapshot 中 `endpoints[]` 数组 ==每项==都是一个独立实例。横向扩容时 `endpoints[]` 含多项；其余字段（`models[]` / `pricing[]` / `accepted_pgws`）仍为 provider 级共享：

```jsonc
{
  "provider_id": "ed25519:<root pubkey>",       // 主键；所有实例共享
  "seq": 17,
  "endpoints": [
    {
      "endpoint_id": "ed25519:<...>",           // 单实例 NodeId
      "region": "geo:eu-west-1",                // 用于 Local Router 亲和
      "node_addr": { "home_relay": "...", "direct_addrs": [...] },
      "chain_addrs": [                           // 该实例用于收款的链上地址
        { "chain": "tempo", "addr": "eip155:4217:0x..." }
      ],
      "capacity": { "concurrent_requests": 4 }
    },
    { "endpoint_id": "ed25519:<...>", "region": "geo:us-east-1", /* ... */ }
  ],
  "models": [...],                               // provider 级共享
  "accepted_pgws": {...}                         // provider 级共享；外层 signed envelope 的 proofs[] 覆盖整份 payload
}
```

`endpoints[]` 长度为 1 = 单实例 Provider（默认）；为 N = 横向扩容。

### 2.4 Local Router 路由

Local Router（[`003`](./003-l3-design.md) §5.2）在选 Provider 候选时：

1. 按模型 / pricing / accepted_pgws 在 provider 维度过滤候选 snapshot。
2. **在选定 snapshot 的 `endpoints[]` 内部**，按 client geographic affinity 优先选最近 `region` 的 `endpoint_id`。
3. 同一 snapshot 内的所有 endpoint 视为可互换的"同一 SLA 单位"（pricing / model 已严格一致——由 snapshot 原子性保证）。
4. 跨不同 `provider_id` 的选择仍按既有打分（价格、延迟、声誉）。

### 2.5 一致性约束（自动满足）

==**snapshot 的原子性已经天然保证**==——`pricing[]` / `models[]` / `accepted_pgws` 是 provider 级字段，被同一份 root proof 覆盖，根本不存在"多 endpoint 字段不一致"这种状态（这是相比旧"多 entry + logical_id"模型的关键简化）。CI 无需额外校验。

---

## 3. Relay 节点横向扩容

==**关键洞察**==：relay 不是 Endpoint，没有 NodeId；它是 URL 标识的服务（`https://relay-us.bitrouter.ai`）。iroh 原生 `RelayMap` 已天然支持多 URL，与本文路径 A 完全正交。

### 3.1 部署模式

- 多个独立 relay URL（`relay-us.bitrouter.ai` / `relay-eu.bitrouter.ai` / `relay-ap.bitrouter.ai`），各自独立 ops。
- 单 URL 内部用 anycast / k8s replica / LB 做 HA——对协议层完全透明。
- 在 BitRouter 公告一个 `relays.json`（或 Registry 中维护）列出官方推荐 URL，让无配置的新节点能拿到默认 RelayMap。

### 3.2 节点端配置

```rust
// BitRouter 二进制启动时
let relay_map = RelayMap::from_iter([
    "https://relay-us.bitrouter.ai".parse()?,
    "https://relay-eu.bitrouter.ai".parse()?,
    "https://relay-ap.bitrouter.ai".parse()?,
]);
let endpoint = iroh::Endpoint::builder()
    .relay_mode(RelayMode::Custom(relay_map))
    .bind().await?;
```

iroh 启动时并发连所有 relay，最快 ack 的成为 `home_relay()` 唯一公告——单 NodeId 限制（[`002` 调研](./002-l1-l2-mvp.md)）下这是当下最优解。

### 3.3 Provider / PGW 实例怎么选 relay

==**与路径 A 天然契合**==：每个独立 NodeId 实例配置完整 RelayMap，启动时自然选最快 relay 作为 home_relay。本地区实例就近本地区 relay。

### 3.4 鉴权与限流（独立于本文）

每个 relay URL 可独立配置 bearer token / 限流 / 监控。参见 [`002` §4 第 1 项](./002-l1-l2-mvp.md)。

---

## 4. PGW 横向扩容（重点：Session Channel 状态）

PGW 与 Provider 在路径 A 下结构对称：每地域独立 `endpoint_id` + region 标签 + 同一份 `pgw_id`-签名的 snapshot 聚合。
但 PGW 多了**链上 channel 状态协调**这个新问题——MPP `session` channel 是有状态的（cumulative_amount, nonce），多实例同时签 voucher 必须避免双花 / 乱序。

### 4.1 Session Channel 状态本质

回顾 [`004-03`](./004-03-pgw-provider-link.md) §2.3：

- Channel 由 `channel_id` 标识，链上锚定 `(payer_chain_addr, payee_chain_addr)`。
- Voucher = `sign((channel_id, cumulative_amount, nonce), payer_chain_secret)`。
- 不变量：`nonce` 严格单调；`cumulative_amount` ≥ 上一条 voucher 的 cumulative_amount。
- payee 端验证：`ecrecover` + 与本地缓存的 `last_seen_(nonce, cumulative_amount)` 比对，违反则 HTTP 402 拒绝。

==**关键**==：`(nonce, cumulative_amount)` 是 **per-channel 的可变状态**——任何能签该 channel 的私钥持有者都必须看到一致的最新值，否则会签出冲突 voucher，被对方拒绝（轻则丢请求，重则在 dispute window 互相挑战）。

==**channel 由 chain_addr 而非 NodeId 标识**==——这是路径 A 设计 PGW 横向扩容的关键支点。

### 4.2 三种状态协调策略

| 策略 | 描述 | 协调成本 | collateral 效率 | 故障半径 |
|---|---|---|---|---|
| **① Sharding（默认）** | 每个 PGW instance 用**独立链上子密钥** = 独立 chain_addr。每 (PGW_i_chain, Provider_j_chain) pair 各开一条 channel。所有状态本地。 | ⭐ 零 | 较低（N×M 总 lockup） | 单实例宕机仅影响其自身的 channel 集 |
| **② Sticky Leasing** | 多实例共享同一链上密钥 = 同一 chain_addr。某 channel 在任一时刻由一个实例通过分布式租约（Redis / etcd / Postgres advisory lock）持有所有权，签 voucher。其他实例代理转发请求到 owner。 | ⭐⭐ 一次租约获取（毫秒级） | 高（1×M 总 lockup） | 切主期间该 channel 暂时不可签（租约 TTL） |
| **③ Shared DB** | 多实例共享密钥 + 共享数据库；每签一条 voucher = 一次 `SELECT ... FOR UPDATE` + 写。 | ⭐⭐⭐ 每 voucher 一次跨网络 DB 锁 | 高 | DB 单点 |

### 4.3 推荐：策略 ① Sharding（默认）

==**协议层默认 PGW 横向扩容方案 = Sharding**==：

```
                ┌──────────────────────────────────────────┐
                │  pgw_id = ed25519:<root pubkey>           │
                │     （"bitrouter-cloud" 是运维备注名）      │
                └──┬─────────────┬─────────────┬───────────┘
                   │             │             │
              PGW endpoint_us  endpoint_eu   endpoint_ap
              chain_addr_us    chain_addr_eu chain_addr_ap   ◄ 独立子密钥
                   │             │             │
              (channels with     (...)         (...)
               regional Providers)
```

理由：

1. **零跨实例协调**——每 channel 的 `(nonce, cumulative_amount)` 永远只被一个实例读写，本地内存即可。
2. **天然故障隔离**——一个 PGW endpoint 宕机，仅它持有的 channel 集进入 close 流程；其他实例完全不受影响。
3. **天然区域亲和**——Local Router 把 us 区 Consumer 路由到 us 区 PGW endpoint（004-01 §3.1 已规定），它再选 us 区 Provider endpoint，channel 就在两个 us 实例之间——延迟最优。
4. **chain_addr 子密钥从 `pgw_id` root key HD 派生**（如 BIP32 / SLIP-10），运维可以审计、可以撤销单个子密钥而不影响 root 主体。
5. **对 Provider 端无感**——Provider 看到的就是"N 个不同 chain_addr 的 PGW counterparty"，每个走标准 1:1 channel，本协议层不需新机制。

### 4.4 备选：策略 ② Sticky Leasing（按需启用，本协议层预留）

策略 ① 的代价是 N×M 个 channel——每条要锁 collateral。当：

- N×M ≫ 实际活跃 channel 数（很多区域间 traffic 很稀）；
- 或单条 channel 的 traffic 集中度太高，需要"PGW 整体 collateral 池"承受短期峰值；

可以切到策略 ②：多 PGW instance 共享同一 chain_addr、共享一池 collateral，但每条具体 channel 的 voucher 签发被路由到唯一 owner instance。

==**协议层无需变化**==——Provider 端依然按 chain_addr 看到 1:1 channel；至于背后是一个进程还是多个进程加分布式锁，是 PGW 内部实现选择。==**唯一约束**==：voucher nonce 跨实例必须严格单调（实现细节由 PGW 自己保证）。

策略 ② 是经典中心化系统的"sticky session" / "consistent-hashing leader" 模式，业界已有大量成熟方案（Redis lock、etcd lease、Cassandra LWT 等），不在本文展开。

### 4.5 备选：策略 ③ Shared DB（不推荐）

每签一次 voucher = 一次跨网络 DB 锁，对 LLM 推理这种高频请求场景延迟与吞吐都不理想（典型 DB 锁 RTT 比单次推理本身 prefill 还长）。仅作为应急或低频场景的 fallback。

### 4.6 多 PGW × 多 Provider 矩阵下的状态视图

设有 `pgw_id` 含 P 个 endpoint、`provider_id` 含 Q 个 endpoint。==**协议层标准视图**==（默认策略 ①）：

```
                Provider ep_1  Provider ep_2  ...  Provider ep_Q
                (chain_p1)     (chain_p2)          (chain_pQ)

PGW ep_1 (g1)   ch_{1,1}       ch_{1,2}       ...  ch_{1,Q}
PGW ep_2 (g2)   ch_{2,1}       ch_{2,2}       ...  ch_{2,Q}
   ...
PGW ep_P (gP)   ch_{P,1}       ch_{P,2}       ...  ch_{P,Q}
```

每个 `ch_{i,j}` 是一条独立 MPP session channel，由 (PGW endpoint_i, Provider endpoint_j) 各自的进程独家维护本地状态。**对一个 channel 的所有 voucher 操作都在同一 (PGW endpoint process, Provider endpoint process) pair 内完成**——这是状态一致性的根本来源。

实际矩阵高度稀疏：多数 channel 因区域亲和路由而长期 idle，可主动 close 释放 collateral；只有 P 与 Q 同区域的 channel 真正活跃。

### 4.7 故障与恢复

| 失败 | 策略 ① 行为 | 策略 ② 行为 |
|---|---|---|
| PGW endpoint 进程宕机 | 它持有的 channel 集进入 close（dispute window 后链上结算） | 租约过期后另一 endpoint 接管；接管前必须读 voucher 共享存储 |
| PGW endpoint 网络分区 | Provider 端一段时间收不到新 voucher → 主动 close 或等恢复 | 同上 |
| Provider endpoint 宕机 | 该 channel 卡住，PGW 看到 voucher ACK 卡住 → 重选 Provider endpoint 走新 channel | 同 ① |
| 整个 `pgw_id` 全 endpoint 宕机 | 所有相关 channel 都进入 close；Provider 待 dispute window 后兑现 | 同 ① |
| 多 endpoint 同时尝试签 voucher（策略 ② 误用） | N/A | 必然冲突——租约层必须保证至多一个 owner |

==**协议层失败语义不变**==——Provider 端的判定永远是"按 (channel_id, nonce, cumulative_amount, signature) 的标准验证"，与 PGW 后端是单 endpoint 还是 P endpoints 无关。

---

## 5. Registry schema 影响汇总

[`003 §2.2`](./003-l3-design.md) Provider / PGW snapshot 内 `endpoints[]` 数组每项支持以下字段（横向扩容相关）：

```jsonc
{
  "endpoint_id": "ed25519:<...>",
  "region": "geo:eu-west-1",                  // ISO / cloud-style region 标签
  "node_addr": { "home_relay": "...", "direct_addrs": [...] },
  "chain_addrs": [                            // 该 endpoint 的链上身份（与 endpoint_id 解耦）
    { "chain": "tempo", "addr": "eip155:4217:0x..." }
  ],
  "capacity": { "concurrent_requests": 4 }
}
```

==**约束（由 snapshot 原子签名天然保证，无需额外 CI 规则）**==：

- 同一 snapshot 内 `endpoints[]` 各项的 `pricing[]` / `models[]` / `accepted_pgws` 自动一致——这些字段在 provider 级，根本不在 endpoint 级声明。
- 同一 snapshot 不同 `endpoints[].region`：合法（横向扩容本意）。
- 同一 snapshot 同 `region` 多个 `endpoints[]`：合法（同区 HA 副本）。
- 同一 snapshot 不同 `endpoints[].chain_addrs`：合法（策略 ① sharding）。
- 同一 snapshot 同 `chain_addrs`：合法（策略 ② leasing）；本协议不要求 Registry 区分两者。

---

## 6. 与既有文档的衔接

| 文档 | 衔接点 |
|---|---|
| [`002`](./002-l1-l2-mvp.md) §4 | 后续工作"跨真机实验矩阵"加备注：路径 A 多区部署用本文设计 |
| [`003`](./003-l3-design.md) §2.2 | snapshot 内 `endpoints[]` 数组每项加 `region` / `chain_addrs[]` 字段（详见 §5）；Local Router 路由打分加"snapshot 内按 region affinity 优先选 endpoint"规则（§2.4） |
| [`004-01`](./004-01-payment-gateway.md) §3.1 | "PGW 与 Provider 长期连接"提到的 capability_grant 应支持同 `pgw_id` 多 endpoint 各自一条；指回本文 §4 |
| [`004-03`](./004-03-pgw-provider-link.md) §2.2 | "channel 是 1:1" 加补充："1:1 是 (chain_addr, chain_addr) 维度；同 `pgw_id` 多 endpoint + 多 chain_addr 子密钥时实际矩阵见 006 §4.6" |
| [`005`](./005-l3-payment.md) §3.5 | PGW 跑路风险已有兜底；同 `pgw_id` 全 endpoint 跑路即"整个 PGW 失联"——风险面同单实例 |

---

## 7. 未决问题

- **`pgw_id` 在链上声誉 / 抵押聚合**：当 `pgw_id` 在 4.1 trust_anchors 中需要展示 collateral 时，是公告"所有子密钥的总锁仓"还是"root key 单独抵押"？snapshot 内可声明一个 anchor 聚合公式。
- **同 `pgw_id` 子密钥旋转 / 撤销**：HD 派生子密钥被泄露时如何在 snapshot 标"已撤销"——机制已有（直接发新 snapshot 替换 `endpoints[]` / `chain_addrs[]`），但子密钥与 endpoint_id 的绑定关系是否需要一个独立的 `revoked_chain_addrs[]` 字段以保留历史可审计性？
- **canonical hash 算法**：[`001-03`](./001-03-protocol-conventions.md) 已规定为 `sha256(JCS(payload))`，字符串形式为 `sha256:<base58btc>`。
- **Local Router 区域感知数据来源**：client 的"地理位置"如何评估？（IP geoIP / 用户配置 / 上次连接 RTT 学习）——超出 P2P 范围，留给 BitRouter 主仓产品决策。
