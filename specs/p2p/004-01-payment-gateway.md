# 004 — L3 支付网关（PGW）：可插拔的中心化支付角色

> 状态：**v1.1 — 正式章节**。
>
> 变更（v1.0 → v1.1）：对齐 [`003 §2`](./003-l3-design.md) 两层身份模型——Provider/PGW Registry 条目以 `provider_id`/`pgw_id`（ed25519 root pubkey）为主键 + `endpoints[]` 数组；capability_grant 由 `pgw_id` root key 签发；附录术语映射表新增旧 EndpointID → 两层身份的对应行。无协议二进制兼容性影响（旧单层 = 新两层的退化情形：root == endpoint）。
>
> 之前版本：**v1.0 — 正式章节**。定义 BitRouter P2P 网络中的"支付网关（Payment Gateway，PGW）"角色及其与 Provider / Consumer 的接口，包括内置 / 外部部署形态、信任模型（合规化与去中心化双轨）、典型部署用例。具体的 MPP 协议绑定与结算实现见 [`005-l3-payment.md`](005-l3-payment.md)。
>
> 关联：[`003-l3-design.md`](./003-l3-design.md) §5（路由）、§6（HTTP 形态）；[`005-l3-payment.md`](005-l3-payment.md)（PGW 路径下 MPP 绑定与 custodial 结算规范）。
>
> 旧版 `004-0-payment-auth-exploration.md` 与 `004-0-payment-gateway-abstraction.md` 已删除/合并，其要点（v0.1/v0.2/v0.3 的演化）已并入 005 附录 A。

---

## 0. TL;DR

- BitRouter Cloud 早期版本承担：**Curated Set 路由 + 5% 网关费 + 多 Provider 聚合 + 对外 SaaS（Stripe/API key/MPP）+ Provider 应付账款记账**。本章把这一组职能抽象成一个**通用、可插拔的角色**——"支付网关"（Payment Gateway，简称 **PGW**）。
- PGW 是 BitRouter L3 的**独立可插拔角色**，有两种部署形态：
  1. **Built-in PGW**：节点二进制内置；与 P2P 节点同进程、同信任域。BitRouter Cloud 是我们运营的默认实例。
  2. **External PGW**：另一个节点（或独立服务）部署，节点与之建立"配置性信任"，通过订阅 / 合约绑定。
- 收益：
  1. ==**支付与路由解耦**==——L3 路由层（003）保持完全去中心化；PGW 层各自承担合规、风控、计费、跨链复杂度。
  2. ==**支付方案可扩展**==——欧盟节点接欧盟合规 PGW；匿名 DeFi 场景接 MPP-channel-only PGW 或 DAO 化纯链上 PGW；企业内网接企业自建 PGW；Provider 联盟跑自治 PGW。互不影响。
  3. ==**协议层保证 PGW↔Provider 性能**==——PGW 与 Provider 是长期、强信任、可复用连接的关系，可以 RFC 化"connection reuse / pre-auth / 延迟关闭"等优化，**不必走每请求一次 402 challenge 的通用流程**。
  4. ==**去中心化集成有协议级默认方案**==——PGW↔Provider 之间天然走 MPP session channel（v0.6 已开源），任何节点都能即开即用、无需法律合同、链上托管。这是 BitRouter 与传统转售 SaaS（OpenRouter / Together AI）模型的本质区别。
- 成本：多了一个角色，多了双向信任配置 UX，多了"哪个 PGW 给我开发票"等运营问题。
- 实施状态：v0.7 已落地（[`005`](005-l3-payment.md) 完成 Cloud → PGW 概念替换、协议层向后兼容）；External PGW SDK 与开源 escrow 参考合约为 v0.7+ / v1+ 跟踪项。

---

## 1. 动机

### 1.1 当前问题：Cloud 是硬编码的特例，限制了网关角色的可扩展性

v0.6 全文用 "Cloud" 这个词来指代支付网关角色。这没有反映一个关键事实：**支付网关本身是一个通用角色，而不是"BitRouter 这家公司"专属的实体**。Cloud 干的事，本质上是**任何想做支付聚合的实体**都能干的事：

| Cloud 现在干的事 | 等价的"通用角色" |
|---|---|
| Curated Provider Set | KYC 联盟、社区组织、行业协会 |
| 5% 网关费 | 商业参数，不同网关 0%–20% 都合理 |
| Stripe / 法币入金 | 任何持牌支付机构 |
| 跨链托管 / 应付账款记账 | DeFi 协议 / 银行 / 企业财务 |
| 对外 OpenAI 兼容 SaaS | 任何想转售推理的转售商 |
| MPP 网关 | MPP 是开放协议 |

### 1.2 为什么不让 Provider 节点自己当网关？

理论上每个 Provider 节点都可以"自己面向 Consumer 直接收钱"——这就是 Pure P2P。但 Pure P2P 有几个长期摩擦：

- **合规黑洞**：Provider 节点要同时管推理、管 KYC、管发票、管退款、管反洗钱。绝大部分 Provider 不想 / 没能力做。
- **链上 UX 黑洞**：Consumer 要管钱包、管 channel、管多链入金。
- **无法做"信用"业务**：周结、月结、信用额度、企业账期——这些都需要一个中介承担信用风险。

**支付网关角色的存在意义就是把这些"非推理"职能从 Provider 节点剥离出去**。Provider 节点专注卖算力 / 卖模型；PGW 专注卖"合规、计费、信用、入金"。

### 1.3 为什么不让 PGW = Provider 的某种 reverse proxy？

PGW 不是单纯的反代。它需要：
- 持有 Consumer 资金（custodial）或代理签名权（non-custodial）；
- 持有 Provider 应付账款；
- 在 Consumer 视野里**作为对手方**出现（开发票、做退款、被监管）。

所以它必须是 L3 协议层认知的角色，不能纯靠 L2 透明转发。

---

## 2. 概念定义

### 2.1 角色：Payment Gateway (PGW)

> **PGW** = 在 L3 协议层被显式认知的、为 Consumer 与 Provider 之间提供"支付聚合 + 信用 + 合规 + 计费"的中介角色。

PGW 至少做这些事：

1. **对 Consumer 暴露统一支付接口**——一种或多种支付方式（API key、MPP、Stripe、微信、银行转账……），Consumer 不必关心 upstream Provider 用什么。
2. **对 Provider 持有应付账款**——PGW 收 Consumer 的钱，按合同（周结 / 月结 / 实时）结算给 Provider。
3. **签发 Order Envelope**——告诉 Provider "这个请求 Consumer 我已经收钱了，毛报价是 X，约定结给你 Y%"。Provider 据此放行推理。
4. **对外承担信用与合规责任**——退款、争议、税务、反洗钱、KYC。

### 2.2 关系：节点（Provider / Consumer / Local Router）vs. PGW

```
    ┌──────────┐                    ┌──────────┐
    │ Consumer │──── L3 路由 ───────│ Provider │
    └──────────┘                    └──────────┘
         │                                │
         │         （003 §5 候选池）         │
         │                                │
         ▼                                ▼
    ┌─────────────────────────────────────────┐
    │              Payment Gateway            │
    │  - 持 Consumer 资金 / 入金通道           │
    │  - 持 Provider 应付账款 / 出金通道       │
    │  - 签 Order Envelope                    │
    │  - 承担信用 / 合规                       │
    └─────────────────────────────────────────┘
```

==**关键**==：PGW 不参与 003 的路由计算。Local Router 仍然按候选池打分自由选 Provider；只是请求落地时，**资金路径**经过 PGW 而非直接 Consumer→Provider。

这与 v0.6 "Cloud 路径 vs. Pure P2P 路径" 的二分相比：

| v0.6 | 抽象后 |
|---|---|
| Cloud 路径 = 用 BitRouter Cloud 做聚合 | 任何 PGW 路径 = 用某个 PGW 做聚合 |
| Pure P2P 路径 = 不经 Cloud | "Direct" 路径 = 不经 PGW（Consumer 直付 Provider） |

### 2.3 两种部署形态

**形态 A — Built-in PGW（节点内置 PGW）**

节点二进制本身就嵌着一个 PGW 实现。节点 owner 同时也是 PGW operator。常见场景：
- BitRouter 官方节点 = BitRouter Cloud（默认实例）。
- 大型企业自部署一套节点 + 自己内部财务作为 PGW（员工内部用）。
- 个人 Provider 也可以"我自己当我自己的 PGW"——其实就是 Pure P2P 退化情形。

信任：**原生信任**。节点与 PGW 同信任域、同密钥、同进程。无须额外配置。

**形态 B — External PGW（外部 PGW）**

节点（一个或多个 Provider）配置信任一个**外部 PGW 实例**——这个 PGW 可能是另一个节点跑的，或者是独立服务（持牌支付公司、DeFi 协议、合规 SaaS）。

信任：**配置性信任**。Provider 在自己的 Registry 公告里声明 "我接受来自 PGW X 的 Order Envelope"，并且与 PGW X 之间有一个**链下合约或链上协议**保证 PGW X 会真的把钱结过来。

```jsonc
// Provider Registry snapshot 片段（草案；与 003 §2.2 schema 对齐）
{
  "provider_id": "ed25519:<root pubkey>",     // 003 §2.1 两层身份模型；Provider 长期身份
  "endpoints": [                               // 多实例 / 横向扩容详见 006
    { "endpoint_id": "ed25519:<...>", "region": "geo:eu-west-1", "node_addr": {...} }
  ],
  "models": [...],
  "accepted_pgws": [
    {
      "pgw_id": "ed25519:<root pubkey>",       // PGW 长期身份（root pubkey），不是 endpoint_id
      "settlement_contract": "ipfs://...",     // 链下合约 hash 或链上 ref
      "max_receivable": "1000.00 USDC"         // 累计欠款上限
    },
    {
      "pgw_id": "self",                        // built-in：我自己当 PGW（= Pure P2P 直收）
      "payment_methods": [                     // 仅 Pure P2P 自收的简化展示；正式 schema = 004-02 §3.2 pricing[] 形态
        { "method": "tempo",  "intent": "session" },
        { "method": "solana", "intent": "charge"  }
      ]
    }
  ],
  "proofs": [{ "...": "bitrouter/proof/ed25519-jcs/0 over the Registry payload" }]
}
```

Consumer / Local Router 在选 Provider 时，可同时选 PGW；约束是 (Provider, PGW) 必须是 Provider 公告里允许的组合。

### 2.4 与 BitRouter Cloud 的关系

BitRouter Cloud 在抽象后变成 **"我们运营的、官方推荐的、默认 Built-in PGW"**——是 PGW 的一个**实例**，不再是协议中的硬编码角色。

- 我们的官方节点：默认装了 Cloud built-in PGW。
- 第三方节点：默认 PGW = self（Pure P2P）；可手动配置 `accepted_payment_gateways = [bitrouter_cloud, ...]` 接 Cloud 的流量。
- 完全独立的 PGW（比如某个欧盟合规 PGW 厂商）：自己注册一个 `pgw_id`（ed25519 root pubkey），发布自己的 PGW snapshot，社区 Provider 自由选择是否接受。

对 005 文档的影响（已在 005 v0.7 落地）：v0.6 文中所有 "Cloud" 替换为 "PGW"；BitRouter Cloud 视为 PGW 的默认实例。

---

## 3. 协议层 PGW↔Provider 优化

这是抽象出 PGW 的最大技术红利之一。Consumer↔Provider 协议层是**每请求一次 402 challenge**（详见 [`005`](005-l3-payment.md) §2）——因为 Consumer 与 Provider 是低信任、短期关系。

PGW↔Provider 不一样：

- **长期关系**——一个 PGW 与签约 Provider 之间一天有上万次请求。
- **强信任** ——双方有合约 / 抵押 / 链上托管约束。
- **批量结算** ——逐请求上链 settlement 没必要。

所以 PGW↔Provider 这一跳可以**走一套完全不同的快速路径**：

### 3.1 持久授权 session（pre-auth）

PGW 与 Provider 在签约时（或定期续签）建立一条**长期 QUIC bidi stream / HTTP keep-alive 连接**，并通过一次性 handshake 完成 mutual auth：

```
PGW → Provider: HELLO + capability_grant 由 PGW root key (pgw_id) 签
                  { pgw_id, endpoint_id, max_concurrent: 100, rate: 1000 req/min,
                    max_credit: 500 USDC, valid_until: T+24h, signature: ... }
Provider → PGW: ACK + acceptance 由 Provider root key (provider_id) 签
                  { provider_id, endpoint_id, signature over capability_grant }
```

==**签名密钥说明**==（与 003 §2.1 两层身份模型一致）：`capability_grant` 由 `pgw_id` root 签名 → 保证整个 PGW 实体的授权能力可跨 endpoint 复用；`endpoint_id` 字段则标明本次 handshake 当前实例（QUIC 会话 / 路由层）。

之后所有请求**复用这一条连接**：

```
[stream 1]  POST /v1/chat/completions  + Order-Envelope: <compact>
            → 直接进推理；不需要 402；不需要每请求 challenge
[stream 2]  POST /v1/chat/completions  + Order-Envelope: <compact>
[stream 3]  ...
```

Order Envelope 可以从 [`005`](005-l3-payment.md) §3.3 定义的"完整 JSON + 签名"压缩成"PGW 此 session 第 N 个 envelope，金额 = X，引用 capability_grant"。

### 3.2 批量结算

每 K 个请求（或每 T 秒）PGW 与 Provider 互发一次累积 settlement summary：

```
PGW → Provider:  SETTLEMENT_TICK
                  { window: [T0, T1], req_count: 1234,
                    cumulative_provider_share: 12.34 USDC,
                    cumulative_pgw_share: 0.65 USDC,
                    signature }
Provider → PGW:  ACK { signature }
```

链上 / 跨链结算降到一天 / 一周一次，不再是每请求一次。

### 3.3 连接复用 vs. 003 的"每请求一条 stream"

003 §5 当前规定 Consumer→Provider "每个请求 = 一条新 QUIC bidi stream"。这是为 Consumer↔Provider 的**短期、低信任**设计的，正确。

PGW↔Provider 不必遵守这一条。可以是：
- 一条长 stream 上 multiplex 多个请求（HTTP/2 / HTTP/3 框架支持）；
- 或一组小型 stream pool（pre-warmed），收到请求时分配空闲 stream。

这是协议层的优化——**Pure P2P 路径不享受**（信任不够），**PGW 路径享受**（长期关系正当性）。

### 3.4 "原生连接复用"是抽象 PGW 的关键卖点之一

> 如果 PGW 不被 L3 协议层认知，PGW↔Provider 的优化就只能靠"约定俗成"或私有扩展，无法标准化。
>
> 把 PGW 立成协议层角色后，可以在 L3 spec 里定义 `bitrouter/pgw/0` 这条专用 ALPN（与 `bitrouter/direct/0` 并列），跑一套 PGW-flavor 的轻量化协议。

---

## 4. 双向信任模型

抽象 PGW 之后，PGW↔Provider 的信任是**双向**的。本节按"合规化（链下法律约束）"vs."去中心化（链上密码学约束）"两条路线分别展开——==实践中两条路线可以混用，比如合规 PGW 也可以叠加链上抵押来增强 Provider 信心；DeFi PGW 也可以要求 Provider 提供链下身份证明==。

### 4.1 PGW → Provider 的信任

PGW 选 Provider 加进自己的 Curated Set 时，需要约束 Provider 在多个维度满足条件。两条路线：

#### 4.1.1 合规化路径（链下法律约束）

| 维度 | 例子 | 承载方式 |
|---|---|---|
| 法律主体 | "你必须是欧盟注册法人 + GDPR 合规" | KYC + DPA 合同 |
| 模型版本 | "你必须跑 vLLM 0.6+ 且模型 hash 匹配" | 商务合同 + 抽查 |
| SLA | "p99 延迟 < 2s, 月可用性 > 99.5%" | SLA 条款 + 监控 + 罚则 |
| 价格 | "毛报价不得低于 Pricing Floor X" | 商务合同 |
| 数据主权 | "推理过程不外发任何 metadata" | DPA + 审计权 |

==**合规 PGW 必须能要求 Provider 也合规**==——这是 PGW 商业模型存在的根本，不能让 Provider 任意接入然后污染整个 PGW 的合规位。承载工具是合同 + KYC + 商业声誉。

#### 4.1.2 去中心化路径（链上密码学约束）

匿名 / 跨境 / DeFi-native 场景下没法签合同。约束改成链上：

| 维度          | 链上承载                                                        |
| ----------- | ----------------------------------------------------------- |
| Provider 身份 | `provider_id`（ed25519 root pubkey）+ 可选的链上 attestation NFT / SBT |
| 模型版本        | Provider 在链上注册 `model_attestation = hash(weights)`，PGW 周期抽查 |
| 抵押 / 押金     | Provider 在 PGW 自定义合约里锁 Y USDC，违约（漏履约 / 报错率超阈值）由 PGW 仲裁罚没    |
| 价格底线        | Pricing Policy（005）签名 + 链上 commit / 公示                      |
| 履约证明        | 由 Provider 给 Consumer 签的 receipt 反向回流到 PGW，作为"我确实推理了"的证据    |


技术承载：Provider 的 Registry 公告（带链上 attestation 引用）+ PGW 自己的 Curated Set 智能合约（白名单 / DAO 投票 / 抵押账本）。

### 4.2 Provider → PGW 的信任

Provider 接受一个 PGW 的 Order Envelope，等于**先垫付推理服务，相信 PGW 之后会真的结款**。Provider 凭什么信？

#### 4.2.1 合规化路径

| 信任来源 | 强度 | 说明 |
|---|---|---|
| **法律合同 + 月结发票** | 中-强 | PGW 签 SLA / 付款合同，违约可走仲裁 / 法律 |
| **声誉 / KYC** | 中 | PGW 是公开法律实体，跑路有法律后果；只对成熟 PGW 适用 |
| **官方背书** | 弱 | BitRouter Foundation 给 PGW 颁发 "trusted" 标签 |
| **集团 / 银行担保** | 强 | 大型 PGW 提供银行履约担保；金融行业标准做法 |

合规化路径下，Provider 通常给 PGW **较大的 `max_receivable` 信用额度**（比如 30 天月结 = 30 天预期流水），因为信任来自合同和法人。

#### 4.2.2 去中心化路径

匿名 PGW 没法签合同——必须把信任**前置成链上抵押 + 即时结算**：

| 信任来源 | 强度 | 说明 |
|---|---|---|
| ==**MPP session 支付通道**== | ★★★ 强 | PGW 与 Provider 开一条 [MPP session channel](https://mpp.dev)（Tempo / Solana），逐请求扣减；PGW 跑路最多损失通道余额（已锁定）。**这是 BitRouter 内置支持的去中心化默认方案**——v0.6 已实现 Tempo/Solana session 服务端代码 + 客户端 SDK，PGW↔Provider 直接复用。 |
| **链上抵押合约** | 强 | PGW 在某个标准 / 自定义合约里锁 N USDC 作担保；超过 N 的应付账款 Provider 拒接。罚没规则可由合约自动执行（见 4.2.3）。 |
| **预付押金** | 中 | PGW 简单转账 N USDC 到 Provider 链上账户，Provider 随用随扣；扣完前 PGW 必须续费。比 MPP channel 简单但有"对手方扣完跑路"风险。 |
| **零信任（pay-per-request）** | 弱 | 退化成 Pure P2P：每请求/每会话独立上链结算（LLM 用 Pure P2P session，非 LLM 可用 charge）。等于不要 PGW。 |

**MPP session channel 是默认推荐方案**，理由：
1. 已是 BitRouter 协议栈一部分（v0.6 开源），PGW 与 Provider 都不必额外开发。
2. 协议层就是 1:1 单收款方——天然适合 PGW↔Provider 这种长期关系。
3. ecrecover 验证微秒级，不需要 Provider 维护数据库（详见 [`005`](005-l3-payment.md) §4.1）。
4. 通道关闭即上链结算，PGW 跑路上限 = 当前未上链 voucher delta，可控。

==**BitRouter 协议默认 / 内置实现**==：任何节点跑 BitRouter 二进制，都自动具备"作为 PGW 与下游 Provider 开 channel"和"作为 Provider 接受上游 PGW 的 channel"两端能力。这是抽象 PGW 的最大去中心化红利之一，也是 BitRouter v0.7 的核心承诺。

#### 4.2.3 双方自建链上合约（高级路径）

对于有特殊需求的 PGW↔Provider 对（比如批量保证金、争议仲裁、多签提现、保险池），双方可以**自建一个智能合约**作为信任锚，把这条 PGW↔Provider 关系所有约束都写进合约：

```
Contract: PgwProviderEscrow
  - pgw: address (PGW chain_addr，由 `pgw_id` HD 派生 — 详见 006 §4)
  - provider: address (Provider chain_addr，由 `provider_id` HD 派生 — 详见 006 §4)
  - pgw_collateral: uint256 (PGW 锁的抵押)
  - provider_pending: uint256 (Provider 未结算应收)
  - settlement_period: uint256 (周结 / 月结)
  - dispute_window: uint256 (Provider 主张未付 → PGW 反驳期)
  - oracle: address (可选第三方仲裁地址)
  - terminate_with_signature(): 双签提前结束 + 自动结算
  - challenge_overdue_payment(): Provider 单边主张超期未付
```

==这是 4.1 / 4.2 各机制的"超集"==——合同条款（4.1.1）、抵押（4.2.2）、罚则（4.1.2）都可以编码进合约。BitRouter 不规定这个合约长什么样，但 v1+ 会发布一个**参考合约模板**（开源、审计过、Tempo/Solana 双部署），让大部分 PGW↔Provider 直接拿来用，复杂用例自己 fork。

参考合约的好处：
- Provider / PGW 双方各自看链上代码就能验证规则，不必读对方 PR。
- 法律约束（如果有）和链上约束**可以并存**——合约保护底线，合同处理灰色地带（如服务质量争议）。
- 允许"半信任"过渡：新 PGW 先用高抵押合约启动，建立声誉后切换到低抵押 / 月结合同模式。

### 4.3 信任模型选择矩阵

| 场景                                 | PGW→Provider        | Provider→PGW                  |
| ---------------------------------- | ------------------- | ----------------------------- |
| 合规企业 PGW                           | KYC + DPA + SLA 合同  | 月结合同 + 银行担保                   |
| 半合规 SaaS PGW（如 BitRouter Cloud 早期） | KYC + 商务合同          | MPP session channel + 公司声誉    |
| DAO / 匿名 PGW                       | 链上 attestation + 抵押 | MPP session channel + 自建合约    |
| 临时 / 低频 PGW                        | 无                   | Pay-per-request（退化为 Pure P2P） |
| 企业内部 PGW                           | 内部组织关系              | 同组织信用                         |

### 4.4 Consumer 对 PGW 的信任

Consumer 视角下，PGW 仍是 custodial 中介。Consumer 必须信 PGW 不卷款跑路。这一边的信任结构：

- **合规化 PGW**：传统 SaaS 信任（OpenRouter / 银行 / 持牌支付机构同款），靠合规与品牌。
- **去中心化 PGW**：必须采用"==链上托管 PGW=="形态——Consumer 入金到合约而不是 PGW 私钥账户，PGW 只能按规则取款。这是 [`005`](005-l3-payment.md) §3.5 第 6 条 "v1+ 链上托管路线"在 PGW 抽象下的自然落地。

本章不展开 Consumer→PGW 的细化信任设计；缓解方案延续 [`005`](005-l3-payment.md) §3.5（强制提现 SLA、链上托管 v1+ 路线）。

---

## 5. 应用场景

### 5.1 默认场景：BitRouter Cloud（built-in，原生信任）

- BitRouter 官方节点 + BitRouter Cloud PGW 同进程；用户体验 = 用现在的 Cloud SaaS。
- PGW↔Provider 信任：合规化路径（KYC + 商务合同）+ 去中心化路径叠加（部分 Provider 走 MPP session channel 即时结算，作为信用补充）。
- 完全等价于 005（v0.7）的描述。

### 5.2 合规场景：欧盟数据主权 PGW

- 一家欧盟公司（"EUPay"）部署一个 PGW，接欧盟支付通道（SEPA / Stripe EU），要求所有 upstream Provider 在欧盟落地、签 GDPR DPA。
- 欧盟的 BitRouter Provider 们公告 `accepted_payment_gateways: [eupay_pgw]`。
- 欧盟用户用 BitRouter 时，Local Router 自动优先选 EUPay 接管的 Provider。
- BitRouter 收 0% 网络费——纯靠 Foundation funding / 协议费用（如果有）支撑路由 Registry。

### 5.3 企业场景：内部 PGW

- 某大型企业部署一组内部 BitRouter 节点 + 内部 PGW（接公司财务 SAP 系统）。
- 员工通过内部 PGW 用模型；财务自动按部门入账。
- 这套 PGW 不对外开放——它的 Curated Set 仅含内部 Provider + 个别外部 SaaS 节点。

### 5.4 去中心化场景 A：MPP-Channel-only PGW（最小去中心化形态）

==**这是与 BitRouter 协议栈契合度最高的去中心化 PGW 形态**==——只用协议自带能力，不需要任何额外合约 / DAO / token。

部署：

- 任何人启动一个 BitRouter 节点，把自己声明为 PGW（节点配置 `role = "pgw"`）。
- 在自己的钱包里准备一笔 USDC（Tempo 或 Solana），主动向 Curated Set 内的每个 Provider 发起 ==MPP session channel 开通==——v0.6 已开源 channel 客户端，调一次 API 即可。每个 Provider channel 锁定金额 = 该 Provider 预期日均流水 × 信任系数（比如 1.5 天）。
- 对外暴露 OpenAI 兼容接口；对 Consumer 走 MPP `session` 收款（也是开 channel）。

运行：

- Consumer 经 PGW 发请求 → PGW 选 Provider → PGW 用与该 Provider 的 channel 即时签 voucher → Provider 验签放行推理。
- 每次请求结算延迟 = 一次 ecrecover ≈ 几十微秒。
- PGW 每天 / 每周关旧 channel 上链结算，重开新 channel 续约。

信任：

- Provider→PGW：channel 已锁余额上限就是 PGW 跑路损失上限（强信任，链上可验）。
- PGW→Provider：传统的"Provider 跑了不交付"风险——可以让 Consumer 凭未交付的 receipt 向 PGW 申诉退款，PGW 累积证据后取消该 Provider Curated Set 资格。**这个风险与 Pure P2P 完全相同**，不是 PGW 引入的新风险。
- Consumer→PGW：本场景下推荐 PGW 也对 Consumer 走 MPP session channel 收款，Consumer 同样不暴露超过 channel 余额的资金。整条链路全程 channel-based，无任何中心化资金托管。

特点：

- ✅ 完全去中心化、无合规要求、跨境无障碍。
- ✅ 利用 BitRouter 内置能力，零额外开发。
- ✅ 适合 Anonymous PGW、Crypto Twitter / DeFi-native 用户群、试验型 PGW、地区性中转 PGW。
- ⚠️ PGW 钱包要管多个 channel（每 Provider 一条 + 每 Consumer 一条）。Channel 管理工具属于 PGW 运营 SDK，BitRouter 应在 v0.7 提供。
- ⚠️ 不接法币入金，仅 crypto。

### 5.5 去中心化场景 B：DAO 化纯链上 PGW

- 一个去中心化协议部署一个"DAO 化的 PGW"——所有规则写智能合约，Curated Set 由 token 持有者投票决定，结算 100% 上链。
- PGW↔Provider 信任：4.2.3 自建合约 + Provider 抵押 + DAO 投票仲裁。
- Consumer 入金到合约（链上托管），按使用量扣费；DAO 协议自动按月分润给 Provider 与 token holder。
- 适合纯加密 native 用户群体；不接 Stripe，不接法币。
- 比 5.4 重得多，但能跑公开市场（token 经济、流动性挖矿等）。

### 5.6 去中心化场景 C：Provider 联盟自治 PGW

- 一群 Provider（比如 GPU 矿池、独立模型 serving 公司）联合起来跑一个 PGW——共享 Curated Set、共享品牌、按贡献分润、自治治理。
- 4.2.3 自建合约里写"成员准入条件 + 分润规则 + 退出机制"。
- 类比：像传统行业的"行业协会"或"互助保险"——成员既是 Provider 也是 PGW 的所有人。
- 这种模型对**没有大资本但想集体合规化**的中小 Provider 很有意义。

### 5.7 去中心化场景 D：节点自营 PGW（PGW = self）

- Pure P2P 的退化情形：Provider 节点自己就是自己的 PGW，不接其他 PGW 的流量、也不通过其他 PGW 收 Consumer 的钱。
- 等价于 005 中的 "Pure P2P 路径"——但在新抽象下也是 PGW 体系的合法部署形态（==**形态退化但模型统一**==）。
- 适合个人 Provider、原型测试、点对点直连。

### 5.8 多 PGW 共存

一个 Provider 可同时被多个 PGW 接入（比如同时接 BitRouter Cloud + EUPay + 一个 DAO PGW），公告里多列几条。一个 Consumer 可选择走任意 PGW（或不走，回退到 Pure P2P 直连）。市场自由竞争，PGW 之间靠 fee rate / 合规位 / 入金通道差异化。

==Provider 多接入多 PGW 是 BitRouter 与传统 OpenRouter / Together AI 等转售 SaaS 的核心区别==——后者下，Provider 接入意味着把流量和品牌都让渡出去；前者下，Provider 仍保持独立 `provider_id`、自己的 Pricing Policy、自己的 Pure P2P 通路，PGW 只是一个可叠加的销售渠道。

---

## 6. 与 003 / 005 的接口

### 6.1 003 修订项（已部分落地）

- §5（路由）：明确"路由计算与支付路径正交"——候选池打分只看推理质量与价格，不看 PGW；选好 Provider 后再走 PGW。
- §6.3（P2P 头）：增加 `BitRouter-PGW-Id` 头（Consumer→Provider，声明本请求经哪个 PGW），Provider 据此查 Order Envelope。
- §6.5（错误码）：增加 `pgw_not_accepted` (Provider 不接受这个 PGW) / `pgw_credit_exhausted` (PGW 在该 Provider 的应付额度满) 等。

### 6.2 005 v0.7 重构（已落地）

005 已按以下结构重组（v0.7）：

- §1 概念把 "Cloud" 换成 "PGW"；Cloud 作为附录单列。
- §2 协议绑定不变（preamble、trailer、错误码）。
- §3 改为 "PGW Internal Settlement"：通用机制（任何 PGW 都用），custodial / 链上托管 / 合约约束作为子方案。
- 新增 §X "PGW↔Provider 快速路径"（本章 §3 提供蓝图）。
- 新增 §X "Provider↔PGW 双向信任配置"（本章 §4 提供蓝图）。
- §7 实现清单按 PGW 角色拆解：bitrouter（节点 + 接 PGW 的 Provider 端） / bitrouter-cloud（Cloud 这个具体 PGW 实例）。

### 6.3 006（Auth）的相关性

规划中的 [`006-l3-auth.md`](./006-l3-auth.md) 节点双密钥（`pgw_id` root + `endpoint_id` per-instance）天然适合 PGW——PGW 也是一个有完整两层身份的"节点"，与 Provider 之间走标准 attestation。Capability grant（§3.1）由 `pgw_id` root key 签发。本章不展开 006。

---

## 7. 开放问题

1. **External PGW 实现路径**：v0.7 已落地 built-in（BitRouter Cloud 实例），后续 External PGW 接口应直接基于 PGW 抽象定义、Cloud 当 reference 实现，避免接口被 Cloud 实现细节污染。
2. **"self PGW" 与 Pure P2P 的关系**：把 Pure P2P 表述成"PGW = self"是否合理？技术上 ok（PGW 退化为零功能中介），但可能让规范多一层无意义的概念。也可以保留 Pure P2P 作为独立路径。
3. **PGW 多链结算的标准化**：Provider 接多个 PGW，每个 PGW 在不同链上结算——Provider 端钱包管理是噩梦。是否需要定义"PGW 必须支持的至少一种通用结算方式"（如 USDC on Solana）？
4. **PGW 注册与发现**：PGW 要不要进同一个 Registry？是否需要"PGW 列表"作为 Registry 一类资源？还是各 PGW 自己宣传、Provider 公告里直接 hardcode `pgw_id`？
5. **协议层强制 PGW 透明度**：要不要在 L3 协议层强制 PGW 必须公开自己的 fee rate、self-dealing 比例、应付账款延迟？还是留给 PGW 自由商业行为 + 市场监督？
6. **Consumer 视角的 PGW 选择 UX**：Consumer 的客户端怎么暴露"用哪个 PGW"？默认从 Local Router 配置取？App 层下推（OpenAI client 的 baseURL 决定）？
7. **PGW↔PGW 互联**：理论上一个 Consumer 经 PGW1 找 Provider，PGW1 不熟 Provider 但 PGW2 熟，PGW1 能不能转给 PGW2 中转？v0 不做（防转包套娃），但应该想清楚为什么不做。
8. **链上托管合约的标准化时机**：[`005`](005-l3-payment.md) §3.5 已经把"链上托管"列为 v1+ 路线；在 PGW 抽象下，这个合约可以做成"任何 PGW 都能用"的开源标准合约（v1+ 跟踪）。
9. **回到合规问题**：PGW 是否需要持牌（MSB / VASP / 银行牌照）？这不是技术问题，但抽象层的设计要让"持牌 PGW"和"非持牌 PGW"在协议里一样跑，靠 Provider/Consumer 各自选择。

---

## 8. 决策与下一步

本章定稿（v1.0）后已完成：

- ==**005 v0.7 落地**==：把 005 文中所有 "Cloud" 替换为 "PGW"；BitRouter Cloud 显式定位为 "PGW 的默认实例"。协议字段、签名格式、错误码均与 005 v0.6 二进制兼容。
- ==**003 同步更新**==：所有 "004" 章节引用根据语义指向 004（PGW 角色）或 005（MPP 绑定）；预留 §6.3 中 PGW 相关 P2P 头与 §6.5 错误码的扩展位。
- ==**MPP session channel 列为 PGW↔Provider 默认机制**==：BitRouter v0.7 二进制原生支持，节点零配置即可作为 PGW 或上游 Provider 一端。

后续工作：

- v0.7 节点 SDK 暴露 `pgw_role` / `provider_role` 双端 API，方便第三方部署 External PGW。
- v1+ 发布开源 PGW↔Provider escrow 参考合约（Tempo + Solana 双部署，审计后开源）。
- 006（Auth）落地后，PGW 的 `pgw_id`（root） + 各 endpoint 的 `endpoint_id`（hot key）+ 链上 Payment Key（HD 派生）三层密钥规范化。

---

## 附录：与现有概念的术语映射

| 旧 (005 v0.6 / 旧 004) | 当前 (本章 + 005 v0.7) |
|---|---|
| BitRouter Cloud | Built-in PGW（默认实例）/ Trusted External PGW |
| Cloud Gateway 路径 | PGW 路径 |
| Pure P2P | Direct 路径（PGW = self / 不经 PGW） |
| Cloud 收 5% 网关费 | PGW 收 X% 网关费（X 由 PGW 自定义） |
| Cloud 的 Order Envelope | PGW 的 Order Envelope |
| Curated Set | PGW 的 Curated Set（每个 PGW 自己一份） |
| `is_house_inventory` | `pgw_self_dealing`（每对 (PGW, Provider) 标注） |
| `max_receivable_to_cloud` | `max_receivable_to_pgw[pgw_id]`（key = PGW root pubkey） |
| Cloud-Provider 月结合同 | PGW-Provider settlement_contract |
| Provider EndpointID（旧单层） | `provider_id`（root） + `endpoint_id`（per-instance），见 003 §2.1 |
| PGW EndpointID（旧单层） | `pgw_id`（root） + `endpoint_id`（per-instance），见 003 §2.1 |
