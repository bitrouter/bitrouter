# 004-02-00 · Auto-Pay API Protocol（自动支付 API 协议）头脑风暴

> **状态**：Brainstorm v0.1。本文是 `004-02` "Inference Payment Protocol" 的**泛化重设**草案，也是后续把 `engineering/p2p-specs/inference-payment-protocol-specs.md` 拆成两层规范的预演。**不是规范本身**——欢迎在评论 / Obsidian 批注中拍砖。
>
> **TL;DR**
>
> 1. 把 `inference payment protocol` 升格为一个**与 LLM 解耦**的、独立的**自动支付 API 协议**：用来让任何"按调用计费、需要随用随付"的 API Provider 公告自己接受哪些自动支付方式。
> 2. **新的 `scheme`** 专门表达 "API 计费方式"（token / weighted_request / duration / ...），与 x402 的 `scheme` 概念无关。
> 3. **新的 `method`** 用 `<上游协议>/<上游协议内的 variant>` 命名（如 `mpp/session`、`x402/v2/upto`、`mpp/charge`、`x402/v2/exact`），与 MPP 自身的 `method`（=`tempo`/`stripe`/...）也无关——后者下沉到 `method_details` 里。
> 4. **彻底干掉"`(scheme, protocol, method, intent, currency, method_details identity 子集)` 六元组"做主键**。改用**命名 offers**：每个 offer 是一个自包含、有名字的 bundle，uniqueness 就是 `name` unique。
> 5. **预留 provider 自定义计费空间**——通过 `scheme = "custom"` + 一个 self-describing 的小 DSL；但不试图覆盖订阅 / 包月这种"非随用随付"模型。
> 6. **GitHub-Copilot-式 "premium request" 加权计费**用 `weighted_request` scheme 一等公民支持。

---

## 0. 这份文档要回答什么

`004-02` 的现有 schema 有三个让人不舒服的地方，需要换骨：

1. **6 元组主键** + per-method "identity 子集 of `method_details`" 是规范里最丑的角落——既难记、又难写 CI、也无法在 JSON 里直观看出"这两条是同一个 offer 的不同表达还是两条不同的 offer"。
2. **`protocol` 和 `method` 两个 tag 的语义混杂**：`protocol = "mpp"` + `method = "tempo"` + `intent = "session"`，但 MPP 自己的"method"概念是支付通道（tempo / stripe / lightning），跟 BitRouter 公告里的"method"是同一个词同一个意思——而我们想让 `method` 成为一个**协议层抽象**（"这是 mpp/session 还是 x402/upto"），把"tempo / stripe"压回 `method_details`。
3. **跟 LLM 推理强耦合**：`Scheme::Token` / Token-only-session 规则、`models[].pricing`、"Solana session 不存在 → 整个 token-based 不可用"……这些都是把"LLM 计费的特殊性"硬编进了协议核心。Embeddings、TTS、image gen、未来的 vector search、agent tool calls，全都需要近似的 "API + 自动支付" 协议，没必要每个垂直领域重新发明一套。

新协议要做的是：**把"按调用计费 + 自动支付"这件事从 LLM 中抽出来，做成一个最小、可扩展、好看的协议**，让 LLM 推理只是它的第一个落地场景。

---

## 1. 范围与非目标

### 1.1 我们要解决的场景

- **按调用计费**的 API：每次调用根据某种可度量的资源消耗收费。
- **需要自动支付**：调用方不能在每次调用前手动批准付款；客户端必须能根据 Provider 公告自动构造支付。
- **金额量级在 cents ~ 几十 USD 之间**：太大要走 KYC / 合规，超出我们想解决的问题；太小则需要 channel / batch（已被覆盖）。
- 典型示例：LLM 推理、embeddings、TTS / STT、image gen / video gen、tool-call API、付费 RAG 检索、付费 RPC 节点、付费爬虫接口……

### 1.2 我们**不**解决

- **订阅 / 包月**：固定费率的访问授权天然不需要"随调用计费"协议，已有现成的 OAuth / API Key + invoice 流程。如果 Provider 想做"$20/月不限量"，请走自家的 billing 后台，不要塞进本协议。
- **链下结算的批发合同 / RFQ**：B2B 大单合同。
- **充值预付式**（pre-paid balance）单独建模：会被 `mpp/session` 覆盖（session 本质就是 deposit 一笔后流式扣减）；但**不**为充值发明独立 `scheme`。
- **GitHub Copilot 整体**：Copilot 自身是订阅式（不在范围）；但 Copilot 内部用来给不同模型/不同操作打权的 "premium request" 度量方式，可以被本协议的 `weighted_request` scheme 表达——这是一个**度量/计费层的复用**，不代表我们要实现订阅。

### 1.3 协议候选名

挑一个：

- **APP** = Auto-Pay API Protocol（短，但跟 "App" 撞）
- **AAPP** = Automated API Payment Protocol（不撞但绕口）
- **PAYG-API** = Pay-As-You-Go API Protocol（直白，但更像 marketing）
- **IPP / IPP-v1** = Inference Payment Protocol（保留旧名，但事实是泛化）
- **AAP** = Automatic API Payment（偏好这个，下文用 AAP）

> 暂用 **AAP** (Automatic API Payment)。最终命名可在 review 时定。

---

## 2. 核心抽象（三层）

AAP 把"一次付费 API 调用"拆成三个完全正交的层：

```
┌─────────────────────────────────────────┐
│  Billing  ── 这次调用花多少 "记账单位"     │   (scheme + rates)
├─────────────────────────────────────────┤
│  Currency ── "记账单位"是什么货币           │   (asset)
├─────────────────────────────────────────┤
│  Rail     ── 这笔钱怎么真的转过去           │   (method + method_details)
└─────────────────────────────────────────┘
```

三层对应三个独立维度：

| 层 | 字段 | 可能取值（v0） | 对应概念 |
|---|---|---|---|
| Billing | `scheme` + `rates` | `token` / `weighted_request` / `request` / `duration` / `bandwidth` / `custom` | "API 怎么计量" |
| Currency | `currency` | `USDC` / `USD` / `BTC-sats` / 任何上游 method 接受的资产标识 | "用什么作为价值尺度" |
| Rail | `method` + `method_details` | `mpp/session` / `mpp/charge` / `x402/v2/exact` / `x402/v2/upto` / `lightning/bolt12-offer` / ... | "钱实际怎么走" |

**关键观察**：现有 schema 把 Currency 揉进了 Rail（`currency` 是 `mpp::ChargeRequest` 的字段）。这是历史包袱——MPP 只是众多 rail 之一，把 currency 放在 rail 里反而堵死了"同一笔逻辑账可以走多个 rail"的表达能力。AAP 把 currency 提到与 rail 平级。

但**实践上**有一类约束跨层：某些 rail 物理上只能接受某些 currency（`x402/v2/exact` 在 base chain 上只能用 base 上的 ERC20）。这种约束由 rail 自己的 schema 表达（在 `method_details` 里），不是 AAP 协议层的硬约束。

---

## 3. `method` 重新设计

> ⚠️ 这是**最重要**的一处命名重构。

### 3.1 旧 vs 新

| 维度                                    | 旧（004-02）                              | 新（AAP）                               |
| ------------------------------------- | -------------------------------------- | ------------------------------------ |
| `protocol` 字段                         | `"mpp"` / `"x402"`                     | **删除**——并入 `method`                  |
| `method` 字段                           | `"tempo"` / `"stripe"`（=MPP 内部 method） | `"mpp/session"` / `"x402/v2/upto"` 等 |
| `intent` 字段                           | `"session"` / `"charge"`               | **删除**——并入 `method`                  |
| MPP 内部 method（tempo/stripe/lightning） | 是 top-level tag                        | 沉到 `method_details.mpp_method`       |
| chain ID / network ID 等               | 散落 `method_details`                     | 仍在 `method_details`（rail-specific）   |

### 3.2 新 method 命名空间

`method` = `<upstream_protocol>/<variant_within_protocol>`，全部小写、`/` 分隔、无版本号时直接用名字、有版本号时嵌入：

| `method` 字符串 | 对应上游 | 含义 | 状态 |
|---|---|---|---|
| `mpp/session` | [MPP](https://mpp.dev) | 长连接 channel，逐请求 voucher | v0 ✅ |
| `mpp/charge` | MPP | 单笔 on-chain charge | v0 (closed source) |
| `mpp/topup` | MPP | 通道续费（依赖上游 ship） | planned |
| `x402/v2/exact` | [x402 v2](https://github.com/coinbase/x402/blob/main/specs/x402-specification-v2.md) | 单笔精确金额 | planned |
| `x402/v2/upto` | x402 v2 | 上限金额，实际按用量结算 | planned |
| `lightning/bolt11` | LN BOLT11 | 一次性 invoice | future |
| `lightning/bolt12-offer` | LN BOLT12 | 复用 offer | future |

**好处**：

- ✅ 单字段 `method` 一看就知道走哪个上游协议、哪个 variant，不用再 `(protocol, intent)` 两段拼。
- ✅ 上游加新 variant 只需新增一个 `method` 字符串和一个 `method_details` 子 schema，不动 enum 结构。
- ✅ MPP 自己的 `method`（tempo / stripe / lightning）下沉到 `method_details.mpp_method`，名字不再撞车。
- ✅ "Solana session 不存在" 不再是协议核心约束——它是 `mpp/session` 的 `method_details.mpp_method` 取值约束（"Solana 不在 session 的合法值集合内"），由 rail-spec 内部表达，AAP 协议层不感知。

### 3.3 `method_details` 的归属

每个 `method` 字符串对应一个 **rail spec**，rail spec 全权定义自己的 `method_details` schema。AAP 协议本身**只**对 `method_details` 做：

- JSON 对象的反序列化检查（必须是合法 JSON object）。
- 透传给客户端，不解释内容。

**没有** "identity 子集进主键" 这种规则——uniqueness 由 §4 的 offer name 解决。

---

## 4. 新顶层结构：Named Offers

### 4.1 设计

```yaml
# 一个 model（或更一般地：一个 endpoint capability）的 offers 列表
offers:
  # 每条 offer 是一个**自包含、有名字、可独立 quote**的支付方案
  - name: tempo-usdc-stream                  # 在本 offers[] 内 unique
    billing:
      scheme: token
      rates:
        input_per_mtok:  "5.00"
        output_per_mtok: "15.00"
    currency: USDC                            # 抽象单位，下面给具体定义
    rail:
      method: mpp/session
      method_details:
        mpp_method: tempo
        chain_id: 4217
        token_address: "0x20c0000000000000000000000000000000000000"
        recipient: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        min_increment: "0.000001"

  - name: stripe-usd-per-call
    billing:
      scheme: request
      rates:
        per_request: "0.06"
    currency: USD
    rail:
      method: mpp/charge
      method_details:
        mpp_method: stripe
        network_id: "acct_1AbCdEfGhIjKlMnO"
        payment_method_types: ["card", "link"]
        max_amount: "10.00"

  - name: copilot-style-premium                # 加权计费示例
    billing:
      scheme: weighted_request
      unit_price: "0.04"                       # 每个 weight 单位的价格
      weights:                                  # 每个 op/model 的 weight
        chat-default:    1
        chat-premium:    5
        agent-tool-call: 2
    currency: USDC
    rail:
      method: x402/v2/upto
      method_details:
        network: base
        asset: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        pay_to: "0xabc..."
        max_amount_required: "100.00"
        max_timeout_s: 300

  - name: lightning-token-streaming
    billing:
      scheme: token
      rates:
        input_per_mtok:  500          # sats per million tokens
        output_per_mtok: 1500
    currency: BTC-sats
    rail:
      method: mpp/session
      method_details:
        mpp_method: lightning
        node_pubkey: "03abc..."
        min_increment_sats: 1
```

### 4.2 为什么这样更优雅

| 旧 schema 的痛点 | 新 schema 的解决 |
|---|---|
| 6 元组主键，per-method identity 子集 | 单字段 `name` unique，CI 一行 `assert_unique(o.name for o in offers)` |
| `(protocol, method, intent)` 三段拼 | `rail.method` 一段字符串 |
| `method_details` 既是配置又是主键 | `method_details` **只**是配置，主键是 `name` |
| `currency` 揉在 rail 里，跨 rail 表达困难 | `currency` 升级为顶层字段，跟 rail 平级 |
| Token-only-session 规则需要扫所有 entry | Token-only-`<某些 method>` 规则在 `scheme: token` 校验时直接 lookup `rail.method` 是否在 streaming-capable 集合内 |
| "同 model 多 entry 但其实是同一个 offer 的不同 quote" 不可能 | 可以为同一 offer 给多个名字（如果有需要）；或一个 name 一个 offer 一目了然 |

### 4.3 Offer name 的命名建议（非强制）

约定俗成（不入协议规范）：

```
<currency>-<rail-shortname>-<billing-flavour>
```

例如 `usdc-tempo-session`、`btc-ln-stream`、`usd-stripe-charge`。Provider 可以加自己的命名空间前缀（如 `aimo/usdc-tempo-session`）来避免社区共享 model 时的冲突。

### 4.4 选择算法（Consumer 侧）

Consumer 拿到 `offers[]`，用以下 pipeline 选一个：

1. **Capability filter**：去掉自家 wallet 不支持的 `rail.method` 或不持有的 `currency`。
2. **Hard policy filter**：去掉超 budget、违反 enterprise compliance 的（如 "禁用 fiat method"）。
3. **Cost estimate**：对剩余 offer 用本次调用的预估 usage（input tokens 已知、output tokens 假设 = max_tokens）算预估总价；按 currency 换算成统一基准（Consumer 自己的 reference currency）做比较。
4. **Tie-break**：优先 streaming（`scheme: token` + `mpp/session`） > on-chain charge > fiat charge。
5. 出价 → 进入 wire 协议（payment-wire-spec）。

AAP **不规定**这个算法；只规定 offers 是 self-contained 可比较的。

---

## 5. Billing schemes 一等公民集合

| `scheme` | `rates` 字段 | 用途 | 推荐 rail |
|---|---|---|---|
| `token` | `input_per_mtok` + `output_per_mtok` (+ optional `cached_input_per_mtok` / `reasoning_per_mtok`) | LLM streaming 推理 | streaming-capable: `mpp/session` / `lightning/bolt12-offer` |
| `weighted_request` | `unit_price` + `weights: { op_name: weight }` | Premium-request 模型（GH Copilot 风）/ 多模型混合定价 / 有内部"积分"概念的 API | 任意 |
| `request` | `per_request` (+ flat `extras: { per_image: ..., per_audio_minute: ... }`) | embeddings、image gen、TTS 单次 | 任意 |
| `duration` | `per_second` | 流媒体推理（音视频生成/对讲）| streaming preferred |
| `bandwidth` | `per_mb` | 反向代理 / 高吞吐 RPC | streaming preferred |
| `tiered` | `tiers: [{up_to: N, per_unit: P}, ...]` + `unit: token\|request\|...` | 阶梯计价 | 任意 |
| `custom` | `custom_meta: { ... }` + `custom_unit_price: ...` + 引用 `custom_metric` | 兜底，让 provider 自定义 metric 名（`per_embedding_dim` 等） | 任意 |

### 5.1 `weighted_request` 详解（GH Copilot 灵感）

```yaml
billing:
  scheme: weighted_request
  unit_price: "0.04"          # 每个 weight 1 单位的价格
  weights:
    chat-gpt5:     1
    chat-gpt5pro:  5          # 一次 premium request 等于 5 次普通
    agent-tool:    2
    code-review:   3
  default_weight: 1            # 当 op_name 未在 weights 中时
```

调用时，Provider 在 response trailer / receipt 里返回 `consumed_weight: <int>`，金额 = `consumed_weight * unit_price`。

**为什么不只用 `request` + 多个 offers**：用 `request` 表达 "5 种不同 op 不同价格" 需要 5 个 offers，且 Consumer 每次都要先决定走哪条 offer，体验差；`weighted_request` 让所有 op 共享一个 channel / 一个支付通道，Provider 内部按 weight 累加，更适合 streaming session 模型。

### 5.2 `tiered` vs `weighted_request`

- `weighted_request`：**按 op 类型** 区分定价，所有 op 同 channel 累加。
- `tiered`：**按累计用量** 区分定价（前 1M token 一个价，之后另一个价），用于鼓励大客户。

二者可以叠加（在 v1+ 考虑），v0 不混用。

### 5.3 `custom` scheme：留给 provider 的逃生舱

```yaml
billing:
  scheme: custom
  metric_name: "per_embedding_dim"
  unit_price: "0.0001"
  metric_url: "https://provider.example.com/specs/per-embedding-dim.md"   # 必填，描述 metric 的语义
```

CI 校验：`metric_name` 必须 snake_case、`metric_url` 必须 https。**不**校验 metric 含义——这是 provider ↔ consumer 的双边契约，跟 `accept-language` 一样：协议给框架，不给词典。

Wire 层（payment-wire-spec）规定 receipt 里必须返回 `consumed: { metric_name: <number> }`，金额 = `consumed[metric_name] * unit_price`。

### 5.4 不在 v0 的 schemes（明确拒绝）

- ❌ `subscription` / `flat_monthly` —— §1.2 已说，超范围。
- ❌ `auction` / `dynamic` —— 实时竞价归 v2+。
- ❌ `bundled` / `package` —— 用多 offers 表达。

---

## 6. Currency 的处理

### 6.1 `currency` 是抽象标识

`currency` 字段只是一个**人类可读的标识符 + Consumer 用来对账的尺度单位**：

- 链上 method 用 token 符号（`USDC` / `USDT` / `BTC-sats` / `ETH-wei`）+ 链上下文在 `method_details` 里（具体合约地址、chain_id）。
- Fiat method 用 ISO 4217（`USD` / `EUR` / `CNY`）。
- Custom 可以用任何 string，但 SHOULD 走 `<chain>-<symbol>` 或 ISO 4217。

**这与现有规范的差异**：现在 `currency` 直接是合约地址（`"0x20c0..."`）。新设计把"人类读得懂的单位"和"链上精确标识"分开——前者放 top-level，后者放 `method_details`。理由：

- Consumer UI 显示 `"USDC"` 比 `"0x20c0..."` 友好得多。
- 链上换合约（USDC issuer 升级）时，`currency: USDC` 不变，只动 `method_details.token_address`。
- 多链同 token（Tempo USDC、Solana USDC、Base USDC）共享 `currency: USDC`，比对账更自然。

### 6.2 仍然保留 currency 的 native unit

`rates` 数字的单位仍然由 `currency` 决定（USDC / USD = 小数；BTC-sats / wei = 整数）。**协议层不做 FX**——这条沿用现有规范。

---

## 7. JSON 完整示例（替代 §3.4 现有示例）

```jsonc
{
  "name": "gpt-4o",
  "offers": [
    {
      "name": "usdc-tempo-stream",
      "billing": {
        "scheme": "token",
        "rates": { "input_per_mtok": "5.00", "output_per_mtok": "15.00" }
      },
      "currency": "USDC",
      "rail": {
        "method": "mpp/session",
        "method_details": {
          "mpp_method": "tempo",
          "chain_id": 4217,
          "token_address": "0x20c0000000000000000000000000000000000000",
          "recipient": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
          "min_increment": "0.000001"
        }
      }
    },
    {
      "name": "usd-stripe-per-call",
      "billing": {
        "scheme": "request",
        "rates": { "per_request": "0.06" }
      },
      "currency": "USD",
      "rail": {
        "method": "mpp/charge",
        "method_details": {
          "mpp_method": "stripe",
          "network_id": "acct_1AbCdEfGhIjKlMnO",
          "payment_method_types": ["card", "link"],
          "max_amount": "10.00"
        }
      }
    },
    {
      "name": "usdc-base-x402-premium",
      "billing": {
        "scheme": "weighted_request",
        "unit_price": "0.04",
        "weights": {
          "chat":          1,
          "chat-premium":  5,
          "agent-tool":    2
        },
        "default_weight": 1
      },
      "currency": "USDC",
      "rail": {
        "method": "x402/v2/upto",
        "method_details": {
          "network": "base",
          "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
          "pay_to": "0xabc...",
          "max_amount_required": "100.00",
          "max_timeout_s": 300
        }
      }
    },
    {
      "name": "btc-ln-stream",
      "billing": {
        "scheme": "token",
        "rates": { "input_per_mtok": 500, "output_per_mtok": 1500 }
      },
      "currency": "BTC-sats",
      "rail": {
        "method": "mpp/session",
        "method_details": {
          "mpp_method": "lightning",
          "node_pubkey": "03abc...",
          "min_increment_sats": 1
        }
      }
    }
  ]
}
```

### 7.1 与旧 schema 直观对比

```diff
- "pricing": [
-   {
-     "scheme":   "token",
-     "rates":    { "input_per_mtok": "5.00", "output_per_mtok": "15.00" },
-     "protocol": "mpp",
-     "method":   "tempo",
-     "currency": "0x20c0000000000000000000000000000000000000",
-     "recipient": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
-     "method_details": { "chain_id": 4217, "fee_payer": true },
-     "intent":   "session",
-     "min_increment": "0.000001"
-   }
- ]
+ "offers": [
+   {
+     "name": "usdc-tempo-stream",
+     "billing": { "scheme": "token", "rates": { ... } },
+     "currency": "USDC",
+     "rail": {
+       "method": "mpp/session",
+       "method_details": { "mpp_method": "tempo", "chain_id": 4217, ... }
+     }
+   }
+ ]
```

视觉上：4 段平铺 → 3 段嵌套 + 1 个 name；信息密度差不多，结构清晰得多。

---

## 8. 与现有 MPP 上游的对接

### 8.1 不再 1:1 镜像 MPP wire 字段名

旧规范追求 `currency` / `recipient` / `method_details` 等字段名"零拷贝塞进 MPP wire"。AAP 放弃这条原则，因为：

- AAP 是**多协议**的（MPP 只是其中之一），强行让 AAP 字段名跟某一个上游对齐，反而会跟其他上游（x402）冲突（x402 用 `network` / `asset` / `pay_to`）。
- "零拷贝"本来就只是一个 `serde_json::to_value(...)` 调用的开销，不重要。
- AAP 引入了 `rail` 抽象——MPP-side 的字段全部归入 `rail.method_details` 子对象，与 x402-side 的字段在结构上隔离。这本身就是一种 namespace。

### 8.2 上游字段映射保留在 rail 实现里

每个 rail（如 `rail/mpp-session.md`）规范里定义自己的 `method_details` 与上游 `mpp::ChargeRequest` 的字段映射；AAP 协议层不感知。这样：

- 上游 `mpp` crate 升级 → 改 `rail/mpp-session.md` 即可，不动 AAP 核心。
- 上游 x402 升 v3 → 新增 `rail/x402-v3-*.md`，与 v2 并存。
- 新增上游协议（LN BOLT12、ERC-7824 channels、…）→ 新增 rail spec，AAP 核心不动。

### 8.3 Cargo / Rust 类型设计示意

```rust
#[derive(Serialize, Deserialize)]
pub struct Offer {
    pub name: String,                        // unique within offers[]
    pub billing: Billing,
    pub currency: String,
    pub rail: Rail,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum Billing {
    Token { rates: TokenRates },
    WeightedRequest {
        unit_price: Decimal,
        weights: BTreeMap<String, u32>,
        #[serde(default = "one")] default_weight: u32,
    },
    Request { rates: RequestRates },
    Duration { rates: DurationRates },
    Bandwidth { rates: BandwidthRates },
    Tiered { unit: TieredUnit, tiers: Vec<TieredTier> },
    Custom {
        metric_name: String,
        unit_price: Decimal,
        metric_url: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct Rail {
    pub method: String,                      // "mpp/session" | "x402/v2/upto" | ...
    /// 透明 JSON，由对应 rail spec 定义 schema。
    /// AAP 层只校验是 object；具体反序列化在 rail-specific 客户端做。
    pub method_details: serde_json::Value,
}
```

注意 `Rail.method` 是 `String` 而非 closed enum——这是有意的，让新 rail 不需要改本核心仓的代码就能被公告（未知 rail 的 offer 在 Consumer 侧会被 §4.4 step 1 自然 filter 掉）。CI 校验 `method` 必须在 Registry 维护的 rail whitelist 内。

---

## 9. CI / Conformance（替换旧的 §4 CR-1..CR-8）

### 9.1 协议层硬约束

| # | 规则 | 说明 |
|---|---|---|
| **C-1** | `offers[].name` 在本 list 内 unique | 唯一规则——干掉了旧 6 元组。 |
| **C-2** | `name` 满足 `^[a-z0-9][a-z0-9/_-]{0,63}$` | URL-safe + Obsidian-link-safe。 |
| **C-3** | `rail.method` 在 Registry 维护的 rail whitelist 内 | 未知 rail 拒绝公告。 |
| **C-4** | `rail.method_details` 是合法 JSON object | 子结构由 rail spec 自校验。 |
| **C-5** | `billing.scheme` 是已知 scheme（含 `custom`） | 未知 scheme 拒绝公告。 |
| **C-6** | `currency` 是 ISO 4217 大写，或 `<symbol>` / `<symbol>-<unit>` 形式 | 仅格式检查；语义由 rail 决定。 |
| **C-7** | `scheme: token` 时 `rail.method` ∈ streaming-capable 集合 | 替换旧 token-only-session；streaming-capable 集合由 Registry 维护，初始 `{mpp/session, lightning/bolt12-offer}`。 |
| **C-8** | `scheme: custom` 时 `metric_url` 必须 https | 防 plaintext/未签名。 |

### 9.2 Rail-specific 校验

委托给 rail spec。例如 `rail/mpp-session.md` 会规定：

- `method_details.mpp_method` ∈ {`tempo`, `stripe`, `lightning`, ...}（rail-specific 白名单）
- 对应每个 mpp_method 的子字段（`chain_id` / `token_address` / `network_id` / ...）类型与值约束
- 反序列化必须通过 `mpp::TempoMethodDetails` 或对应类型

AAP CI 调用对应 rail 的 validator（rail spec 里发布 validator 实现）。

---

## 10. 与 Spec-v0 的关系

### 10.1 插槽归属

`spec-v0.md` §5.1 现有 slot：

```
models[].pricing  ── 由 inference-payment-protocol-spec 拥有
```

改成：

```
models[].offers   ── 由 auto-pay-api-protocol-spec (AAP) 拥有
```

字段名 `pricing` → `offers` 是 breaking change，但因为 v0 协议尚未广泛部署，可以直接换。如果不想换，至少把内部 entry 结构换成本文 §4 的形态，字段名继续叫 `pricing[]`。

### 10.2 Sibling spec 拆分

由现在的：

- `inference-payment-protocol-specs.md`（一篇覆盖 LLM 计费 + MPP rail 细节）

拆成：

- `auto-pay-api-protocol-spec.md`（AAP 核心：三层抽象、Named Offers、`scheme` / `currency` / `rail` 框架）
- `rail/mpp-session-spec.md`（MPP session rail 细节，含 mpp_method 白名单）
- `rail/mpp-charge-spec.md`
- `rail/x402-v2-spec.md`（含 `exact` 与 `upto` 两个 method）
- `rail/lightning-bolt12-spec.md`
- `billing/weighted-request-guide.md`（非规范，给 Provider 用 weighted_request 的 best practice）

LLM 推理 specifically 的内容（"为什么 token 必须 streaming"、"Solana session 不存在的过渡方案"）**保留**在原 `inference-payment-protocol-specs.md`，但变成 AAP 在 LLM 场景下的**应用 profile**——只剩薄薄一层 LLM-specific 约束 + 引用 AAP。

---

## 11. 开放问题 / 拍砖区

1. **协议名**：APP / AAPP / PAYG-API / IPP / AAP？
2. **`offers` vs `pricing` 字段名**：要不要 breaking？
3. **Registry-side rail whitelist 的治理**：是 PR-based（社区维护）还是 issuer-claimed（rail 作者公告）？
4. **`weighted_request` 的 `weights` 键名是否应 namespace**：`gpt5/chat-premium` vs 裸 `chat-premium`？防止跨 model offer 复用时撞名。
5. **`custom` scheme 滥用风险**：是否要求 `custom_url` 指向的文档必须有某种 schema header（如机器可读的 OpenAPI extension）？
6. **多 currency 同 offer**：要不要支持 "这个 offer 接受 USDC 或 USDT，by consumer choice"？v0 倾向不支持（用多个 offers 表达）。
7. **Rail capability 声明**：Provider 怎么告诉客户端"我支持 mpp/session 但只接受 tempo 不接受 stripe"？目前靠 `rail.method_details.mpp_method` 隐式表达，是否需要显式 `rail.capabilities` 字段？
8. **协议版本号**：AAP 自己要不要 `version: 1` 字段？还是跟 `bitrouter/p2p/0` 走？
9. **签名范围**：snapshot-level 签名覆盖 `offers[]` 即可，还是每个 offer 也允许独立签名（让多 issuer 共同 endorse 一个 endpoint）？
10. **跨 endpoint 共享 offers**：常见的 case 是 provider 对所有 model 给同一套 payment offers，要不要支持 endpoint-level `default_offers` + model-level `offer_overrides`？v1+ 考虑。
11. **跟 `models[]` 的耦合**：一个 endpoint 的 `offers` 是否一定要 per-model？还是允许 endpoint-level 一套 offers 覆盖所有 model（节省 snapshot 空间）？
12. **Receipt schema**：weighted_request / custom 必须在 receipt 里返回 `consumed_weight` / `consumed[metric_name]`——这条要不要由 AAP 规定还是 wire-spec 规定？倾向 AAP 规定（属于 billing semantics），wire-spec 实现。

---

## 12. Next steps（如果方向被确认）

1. 把 `engineering/p2p-specs/inference-payment-protocol-specs.md` **冻结**为 LLM application profile，新建 `auto-pay-api-protocol-spec.md` 承接核心。
2. 起草 `rail/` 目录下的若干 rail spec，从 `mpp-session` / `mpp-charge` 开始。
3. 改 `spec-v0.md` §5.1 / §10 的 slot 归属与 sibling spec 表。
4. 让 `bitrouter` repo 里 publish offer 的代码与 v0 的兼容层并存一段时间，再切换。

---

## 附录 A · 与 x402 v2 spec 的关键差异

| 维度 | x402 v2 | AAP |
|---|---|---|
| 范围 | HTTP 单次请求级支付 | API 公告 + 自动支付协商 |
| 计费模型 | `exact` / `upto` 两种金额上限模型 | 完整的 `scheme` 系列（token/weighted/duration/...） |
| 支付协议 | 内置的 EVM "exact"/"upto" + 可扩展 | 通过 `rail.method` 引用任意上游协议（含 x402 自身） |
| 数据 envelope | `payment_requirements` 数组 | `offers[]` named bundle |
| `scheme` 含义 | 支付协议的 variant（`exact` / `upto`）| API 计费方式（`token` / `request` / ...） |
| Wire 形态 | `WWW-Authenticate: x-payment-required` | 委托给 `rail` spec（多协议） |

x402 是个好 rail；AAP 把它当 rail，不与之竞争。

---

## 附录 B · 与 GitHub Copilot premium request 的对应

GH Copilot 内部用 "premium request" 度量不同模型 / 不同操作的成本（如普通 chat = 1 unit、premium chat = 5 units）。AAP 用 `weighted_request` scheme **直接表达**这套度量：

```yaml
billing:
  scheme: weighted_request
  unit_price: "0.04"
  weights:
    chat:         1
    chat-premium: 5
    agent-step:   2
```

**注意**：Copilot 自身仍是订阅制（不在 §1.2 范围内），AAP 借鉴的只是其 metering 模型。如果未来某个 Copilot-like 服务想做 "随用随付的 premium request"，AAP 直接就能表达。
