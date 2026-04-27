# 007-02 — v0 原型回流：协议改进草案

> 状态：**v0.1 — 草案**。本文基于第一轮原型实现（commit `46fc695`，详见 [`bitrouter-p2p-proto/docs/V0_IMPLEMENTATION_REPORT.md`](../../../code/bitrouter/bitrouter-p2p-proto/docs/V0_IMPLEMENTATION_REPORT.md)）对 P2P 协议规范（[`003`](./003-l3-design.md) / [`004-01`](./004-01-payment-gateway.md) / [`004-02`](./004-02-payment-protocol.md) / [`004-03`](./004-03-pgw-provider-link.md) / [`005`](./005-l3-payment.md)）提出**协议层面**的修订建议。
>
> 范围：**只讨论 wire format / 字段语义 / 校验规则 / 互操作约束**；不包含纯工程改进（TLS termination、持久化、并发压测、shell harness 等——这些在 PRD 演进与实现仓的工程 backlog 中跟踪）。
>
> 目标读者：协议规范维护者、未来 v1 演进决策者。本协议**尚处于内部规范编写阶段，无任何已发布的对外承诺**——所有规范修订直接覆盖现行文本，不引入兼容期、不保留 legacy 形态、不为"已部署实例"留过渡 ALPN/字段。原型实现作为内部脚手架配合协议同步替换。
>
> 术语遵循 [`001-02-terms`](./001-02-terms.md)。

---

## 0. TL;DR

原型跑通后暴露出 **12 项需要回流到协议规范的改动**，按修订面分类：

| #   | 主题                                                                                                                                                  | 受影响文档      | 改动面       | 适用 leg |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ---------- | --------- | ------ |
| R11 | ==**协议三段 leg 分层**==：Leg A (Consumer↔Provider Direct) / Leg B (Provider↔PGW) / Leg C (PGW↔Consumer)；每段 wire 兼容性约束与支付模型分别规约 | 003、004-03、005 | wording / 统领 | — |
| R1  | 金额表示：结算锚定 token 原生 atomic unit + 报价用有理数                                                                                                              | 004-02、005 | schema    | A、B |
| R2  | Provider/PGW 间应用层切到 HTTP/3 over QUIC（单 ALPN）                                                                                                         | 003、005    | transport | A、B |
| R3  | Leg A 流式响应：HTTP/1.1 兼容 SSE body + `Payment-Receipt` HTTP trailer + GET 回执回退（标准 MPP Receipt 结构）                                                       | 005        | schema    | A |
| R4  | 协议错误模型分层：支付类走 MPP `WWW-Authenticate: Payment` + 402；其他类走 RFC 9457 Problem Details                                                                    | 003、005    | schema    | A、B |
| R5  | 身份字符串编码统一锁定（z-base32 + algo 命名空间）                                                                                                                    | 001-02、003 | wording   | A、B |
| R6  | Provider 必检项扩展（新增 digest / expires / pricing hash / token limit / fee split）                                                                         | 004-03、005 | wording   | A、B |
| R7  | `Payment-Receipt`（原 Settlement Trailer）"Provider 重签"规则显式化                                                                                            | 005        | wording   | A |
| R8  | MPP `Authorization: Payment` / `WWW-Authenticate: Payment` wire 形态严格对齐（id / realm / method / intent / request / digest / expires / opaque + JCS + HMAC-bound id） | 005        | schema    | A（B 仅 fallback） |
| R9  | Tempo session voucher 改为 EIP-712 typed data + TIP-20 base units，secp256k1 signer                                                                     | 004-02、005 | schema    | A |
| R10 | BitRouter Order context 收敛为 MPP Credential `payload.order` extension，移除独立 `Order-Envelope` header                                                     | 004-03、005 | schema    | A |
| R12 | ==**Leg B 支付控制平面解耦**==：独立 QUIC 连接承载长期渠道 + 累计型 voucher（epoch 结算）；LLM stream 仅携 `order_ref`，wire body 与支付完全脱钩                                    | 004-03、005 | schema / transport | B |

==**R11 是统领项**==，R3 / R7 / R8 / R9 / R10 / R12 的适用范围由 R11 划定。落地顺序建议：(1) 先合 R11（纯 wording 框架）；(2) 再合 R5 / R6 / R7 / R1 / R2（基础设施层）；(3) 最后合 Leg A 收敛批次 R3 + R4 + R8 + R9 + R10 与 Leg B 解耦批次 R12（==互不阻塞，可并行落地==）。所有修订均**直接覆盖**现行规范文本，不保留旧形态、不引入兼容期。


---

## 1. 背景

v0 原型完成了端到端的 Direct 与 PGW 路径验证（详见报告 §6），证明了 [`003`](./003-l3-design.md) / [`004-01`](./004-01-payment-gateway.md) / [`004-02`](./004-02-payment-protocol.md) / [`004-03`](./004-03-pgw-provider-link.md) / [`005`](./005-l3-payment.md) 的总体设计**可连通**。但实现过程中也暴露出若干**协议规范本身**的不足：有些是规范留白被实现填上了"看起来对但其实未冻结"的细节（如 trailer 再签名规则）；有些是规范在实现前未充分考虑互操作（如金额仍以 decimal 字符串/`f64` 流通）；有些是 v0 原型为了聚焦 L3 闭环而临时妥协（如自定义 framing），但长期不应留作官方协议形态。

本文逐项给出**最小修订建议**。==协议尚处于内部规范编写阶段==，无任何对外承诺，==所有修订直接覆盖现行规范文本==——不保留旧字段、不引入 deprecation 期、不为已部署原型留过渡 ALPN/形态；原型实现与规范同步替换。所有具体字段、JSON 模板、ABNF 等留待对应规范文档下一版本承接，本文只锁定**方向**和**约束**。

---

## 2. R1 — 金额表示锁定为 integer atomic unit

### 2.1 问题

当前 [`004-02`](./004-02-payment-protocol.md) 与 [`005`](./005-l3-payment.md) 中 `pricing[].rate`、`quote.amount`、`voucher.cumulative`、`settlement_trailer.actual_amount` 等字段以**人类可读 decimal 字符串**呈现（如 `"0.0001"`）。原型实现报告 §8.4 指出：

- 当前 `compute_actual_amount` 与 PGW quote 仍存在 `f64` 路径；
- decimal 字符串输出在不同语言/不同库下不稳定（trailing zero、normalization、rounding mode）；
- 多实现互操作时，对账边界 case 会出错；
- 报告将其列为 P0 技术债。

但根因不只是工程实现，而是**协议规范本身没有冻结金额的二进制语义**：现行文本只规定"语义为某种 token 数额"，未规定"在 wire 上以何种精确表示流通"。

### 2.2 关键拆分：结算精度 ≠ 报价精度

协议中存在**两类精度需求完全不同**的金额字段，统一用单一表示会两边都得罪：

| 类别 | 例子 | 精度需求来源 | source of truth |
|---|---|---|---|
| **结算金额** | `voucher.cumulative`、`settlement_trailer.actual_amount`、`top_up.amount` | 链上代币合约本身（落账分辨率） | 代币 `decimals()`（USDC=6, DAI=18, BTC=8 …） |
| **报价/费率** | `pricing[].rate`（per-token、per-call、per-ms） | 业务定价分辨率（可远小于 1 atomic unit） | 协议规范允许的最大表达力 |

USDC 的 6 dp 对结算是伪问题（链上就是 6 dp，多一位也无法落账），对报价才是真问题（`$1e-7/token` 类 sub-cent 单价 6 dp 不够）。结论：**结算锚定 token 原生 dp，报价单独走有理数路径**，二者在结算边界一次性收敛。

### 2.3 建议

锁定如下规范（建议进入 [`004-02`](./004-02-payment-protocol.md) v0.8）：

- **结算字段**（`voucher.cumulative`、`settlement_trailer.actual_amount`、`top_up.amount`、未来同类字段）：以引用 token 的**链上原生 atomic unit** 为 wire 单位，整数字符串表示。==**协议规范不指定全局 dp**==——权威永远是代币合约。
- **token decimals 元数据**：写在 Provider snapshot 的 `pricing[].token` 项内（CAIP-19 + 显式 `decimals` 字段一并固化），避免运行时查链；CI 校验该 `decimals` 与 CAIP-19 实际链上代币一致。
- **报价/费率字段**：以**有理数 rational** 表达，`{numerator, denominator}` 两者均为 atomic-unit 域的非负整数字符串。例：
  ```json
  "pricing": [{
    "intent": "session",
    "model": "gpt-x",
    "token": { "asset": "eip155:1/erc20:0xA0b8...USDC", "decimals": 6 },
    "rate": {
      "kind": "per_unit_usage",
      "unit": "output_token",
      "numerator": "1",        // 1 atomic-USDC ...
      "denominator": "10000"   // ... per 10000 output tokens
    }
  }]
  ```
  分母可任意放大，报价精度无上界；不引入"协议级 dp"，不需要在 6/9/18 之间做痛苦折衷。
- **整数表示规则**：所有金额字段（含 numerator / denominator）以 JSON string 表示（避免 JS number 精度损失）；禁止小数点、禁止科学计数法、禁止前导零（`"0"` 除外）；denominator ≠ `"0"`。
- **结算计算路径**：big-int 全程；**唯一的舍入**发生在 `quote_atomic = ceil(numerator * usage_units / denominator)`，方向 normative 为 **`ceil`**（保护 Provider，避免 dust 亏损）。其后 `cumulative_after = cumulative_before + quote_atomic` 等对账等式在 atomic-unit 整数空间内**严格成立、禁止再舍入**。
- **展示层**：仅 CLI / 文档示例做 atomic-unit → decimal 反向呈现；wire 上禁止出现 decimal。

### 2.4 落地影响

- 规范侧：[`004-02`](./004-02-payment-protocol.md) 直接修订 pricing / 金额字段定义；现行文本中的 decimal 字符串示例全部替换。Provider snapshot 中 `pricing[].token` 由字符串升级为对象形态以承载 `decimals`。
- 实现侧：原型 `bitrouter-mpp` / `bitrouter-cli` 中的 `f64` 路径全部替换为 `u128` / `BigUint`；`compute_actual_amount` 重写为 big-int + `ceil`。所有现行 fixture、snapshot、example 重写并重签一次。
- CI rules 增加：
  - 结算字段 schema = `pattern: "^(0|[1-9][0-9]*)$"`；
  - rate schema = `{numerator: <int-string>, denominator: <non-zero-int-string>}`；
  - `pricing[].token.decimals` 与 CAIP-19 链上 `decimals()` 一致性校验；
  - `quote_atomic` 计算结果具有 reference vector。

### 2.5 备选方案与拒绝理由

- **协议层统一 decimals（global N，如 9 或 18）**：多代币原生 dp 不一致（USDC=6, DAI=18, BTC=8…），无论 N 取何值，链上结算永远要在边界做缩放与舍入；选 9 在稳定币生态外缘走，互操作更脆弱。**拒绝**。
- **每个 amount 字段 inline `{value, decimals}`**：与 token 元数据冗余；snapshot 内部出现"某条 pricing 写 9 dp、voucher 写 6 dp"的合法但语义错误情形，校验代码难以发现；canonical hash 后续升级也更脆。**拒绝**。
- **用 `rust_decimal` / `bigdecimal` 直传 decimal 字符串**：依赖各语言 decimal 字符串规范化差异，对账边界 case 仍易错。**拒绝**。
- **浮点 + 容差**：付款语义不容许容差。**拒绝**。

### 2.6 行业参考

- **Lightning Network**：路由计费精度用 millisatoshi（msat = sat × 1000），结算回到 sat（BTC 原生单位）——"报价精度 ≠ 结算精度，结算锚定原生 dp" 同构。
- **Stripe**：始终以最小货币单位（cents）整数表达 amount，不引入"协议 decimals"。
- **ERC-4626**：通过 share/asset rate 解耦"内部精度"与"外部代币精度"，思路一致。

---

## 3. R2 — Provider/PGW 间应用层切到 HTTP/3 over QUIC（单 ALPN）

### 3.1 问题

当前 [`003`](./003-l3-design.md) / [`005`](./005-l3-payment.md) 规定 ALPN = `bitrouter/p2p/0`，并通过 iroh QUIC connection 在该 ALPN 上承载请求 / 响应。原型实现报告 §8.13 指出：实际跑通的是**自定义 request/response framing** + 自定义 SSE 投递，并非 HTTP/3。

虽然这在 v0 起步阶段是合理的工程取舍（避免 h3 stack、ALPN 多路、headers/trailers/streaming body 集成同时引入），但**协议规范不应把自定义 framing 写死为长期形态**：

- 自定义 framing 等于在 QUIC 之上重新发明 HTTP 的 headers / status / trailers / streaming；
- SSE chunk、partial frame、错误事件难以与 OpenAI 生态互通；
- 未来支持 embeddings / image / audio / tools 等更多 endpoint 时扩展成本指数上升；
- 标准 HTTP/3 stack 在代理、观测、debug、middleware、安全策略上更成熟。

### 3.2 建议

==**协议尚处于内部规范编写阶段，没有任何已发布的对外承诺，因此不引入双 ALPN 过渡层。直接把 ALPN `bitrouter/p2p/0` 的语义重定义为 HTTP/3 over QUIC。**==

修订内容（建议进入 [`003`](./003-l3-design.md) §（transport）与 [`005`](./005-l3-payment.md) §（应用层））：

- ALPN 字符串保持 `bitrouter/p2p/0`，==**语义改为 HTTP/3**==；不新增 ALPN，不保留旧自定义 framing 的协议地位。
- 原型实现中的自定义 framing 标记为**实现阶段一次性脚手架**（implementation scaffold），不是协议正式形态；下一轮实现直接替换为 HTTP/3，无 deprecation 期。
- 现行 [`005`](./005-l3-payment.md) 中的下列对象由 HTTP/3 标准语义承载（不再是自定义 frame 字段）：
  - `Order-Envelope` → 标准 HTTP request header（base64url(canonical-json) + 签名）。
  - `X-Channel-Voucher` → 标准 HTTP request header（JWS compact）。
  - `Settlement-Trailer` → 由 R3 + R7 + R8 重构为 MPP 标准 `Payment-Receipt`，作为 HTTP **trailer**（`Trailer: Payment-Receipt` 头预声明 + 流尾 trailer 帧）；==同时提供 `GET /v1/payments/receipts/{challenge_id}` 回执回退端点==，覆盖反代/客户端 SDK 不读 trailer 的场景。详见 R3 / R7。
  - 流式响应 → 标准 HTTP response body，content-type `text/event-stream`，遵循 R3 的 HTTP/1.1-兼容 SSE 形态。
- 未来若出现"必须低于 HTTP 抽象"的协议消息（暂未识别到），同 ALPN 内通过以下方式承载，**仍不引入第二个 ALPN**：
  - HTTP/3 DATAGRAM（RFC 9297）用于非可靠低级别信号；
  - 保留路径前缀 `/_bitrouter/control/...` 用于控制平面 HTTP request；
  - 必要时 WebTransport over HTTP/3 用于长连双向流。
- snapshot 中 `endpoints[].alpn` 字段保持单值 `bitrouter/p2p/0`，==**不引入 `alpn[]` 列表字段**==；如未来真要分裂 ALPN 再升级 schema，但本轮明确不做。

### 3.3 落地影响

- 规范侧：[`003`](./003-l3-design.md) 与 [`005`](./005-l3-payment.md) 中"自定义 framing / 自定义 SSE 投递"段落整体替换为"HTTP/3 over QUIC + 标准 headers / trailers"段落。
- 实现侧：自定义 framing 代码路径删除而非保留；HTTP/3 stack 一次性接入。报告 §10 已将 HTTP/3 列为 P1，本节将其升级为规范的强制项。
- snapshot schema 不变（`endpoints[].alpn` 仍是单值字符串 `"bitrouter/p2p/0"`，仅语义被重定义）。
- Registry CI 增加 R 规则：`endpoints[].alpn == "bitrouter/p2p/0"`（唯一允许值）。

### 3.4 备选方案与拒绝理由

- **保留双 ALPN（旧自定义 framing + HTTP/3 并存）**：仅为兼容性服务，但本协议尚无对外用户，复杂性纯亏损。**拒绝**。
- **继续维护自定义 framing 长期化**：放弃与 OpenAI 生态互通，长期维护成本高。**拒绝**。
- **保留旧 ALPN 字符串作 legacy、新增 `bitrouter/p2p/h3/0` 作主**：增加一个永远不该被 dial 的字符串，无收益。**拒绝**。

---

## 4. R3 — 流式响应规范化（HTTP/1.1 兼容 SSE + `Payment-Receipt` trailer + GET 回退）

### 4.1 问题

原型实现报告 §8.10 / §9.5 指出：CLI 当前兼容 OpenAI 风格 `choices[0].delta.content` 与 mock upstream 的 `delta_text` 两种 SSE shape；usage / 结算 / error event 还没有系统化。当前 [`005`](./005-l3-payment.md) 对**响应流的 wire format** 仅描述了"SSE / 流式 + 末尾 settlement trailer"，并未规定：

- 流帧形态（事件名 vs 匿名 + JSON 判别）；
- usage 上报形态；
- 错误事件 schema；
- 结算对象的承载方式与回退通道。

==此外，Direct path 上 Consumer 完全可能就是一个套了 OpenAI 客户端 SDK 的 agent==——SSE body 必须直接可被通用 OpenAI 兼容消费者解析，结算信息**不可污染** SSE chunk schema。

### 4.2 关键观察：MPP 标准把 receipt 锚定在 HTTP 头/尾上

[MPP HTTP Transport 规范](https://mpp.dev/protocol/transports/http) 把 `Payment-Receipt` 定为响应头：非流式响应直接放 response header，流式响应放 HTTP **trailer**。MPP `Receipt` 结构（[`/protocol/receipts`](https://mpp.dev/protocol/receipts)）是 base64url(JSON)，固定字段集 `{ challengeId, method, reference, settlement: { amount, currency }, status, timestamp }`，session intent 下 `reference = channelId`。

第三方 AI SDK（实证 Vercel AI SDK `parseJsonEventStream` + `chunkBaseSchema = z.looseObject`，见 [`packages/openai-compatible`](https://github.com/vercel/ai/tree/main/packages/openai-compatible)）忽略 SSE `event:` 字段；自定义 SSE 事件名零兼容性收益且会让 chunk schema 不匹配触发错误路径。`data: [DONE]` 被 Vercel AI SDK 静默忽略，但被 OpenAI 官方 SDK 与多个轻量 client 当作终止哨兵——保留无害且必要。

HTTP trailer 在浏览器 `fetch` API、不读 trailer 的 OpenAI SDK、绝大多数反代上会被丢失，这是 `Payment-Receipt` 的真实交付风险。但**不应**因此把结算信息内嵌进 OpenAI SSE chunk（污染上游 schema、要求所有 PGW 在转发时剥离子对象、与未来非 chat endpoint 不通用）；正确方案是**双通道**：trailer 优先，`GET` 回执回退覆盖 trailer 不可达场景——这与 [`005`](./005-l3-payment.md) §2.5 既有的 fallback 设计方向一致，仅需细化字段集合至 MPP 标准 Receipt。

### 4.3 建议

在 [`005`](./005-l3-payment.md) "响应流规范"小节锁定如下形态：

- **SSE body 严格保持 OpenAI v1 chat completions shape，==不携带任何 BitRouter-specific 字段==**：
  - 所有帧均为匿名 SSE (`data: <json>\n\n`)，**不使用** `event:` 字段。
  - 增量内容帧：`{"id","object":"chat.completion.chunk","created","model","choices":[{"index","delta":{"content"},"finish_reason":null}]}`。
  - 最终 usage 帧（OpenAI `stream_options.include_usage` 等价）：`{"id","object":"chat.completion.chunk","model","choices":[{"index","delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens","completion_tokens","total_tokens"}}`。
  - 流终止哨兵：`data: [DONE]\n\n`，**必须**作为最后一帧。
  - content-type：`text/event-stream; charset=utf-8`。
- **结算（`Payment-Receipt`）走 HTTP trailer + GET 回退双通道**，==不嵌入 SSE body==：
  - **主通道（trailer）**：响应 headers 包含 `Trailer: Payment-Receipt, Payment-Receipt-Sig`；流尾 trailer：
    ```
    Payment-Receipt: <base64url(JCS(receipt_json))>
    Payment-Receipt-Sig: <base64url(sig)>
    ```
    `receipt_json` 字段集与 [MPP `/protocol/receipts`](https://mpp.dev/protocol/receipts) 完全一致：`{ challengeId, method, intent, reference, settlement: { amount, currency }, status, timestamp }`，外加 BitRouter `order` 扩展（按 R10），并按 R7 由 Provider 重签。
  - **回退通道（GET 轮询）**：客户端凭 `challenge_id` 调用 `GET /v1/payments/receipts/{challenge_id}`；返回 `200 OK` + 同一份 `Payment-Receipt` header + 受签 JSON body（content-type `application/json`）。Provider 必须为每个已结算请求保留 receipt **≥ 24h**（具体 TTL 由 PGW / Provider 配置）；查询不存在的 receipt 返回 `404` + RFC 9457 problem+json（按 R4）。
  - **不**使用 SSE body 内自定义结算帧、不使用 `usage.bitrouter.*` 扩展、不使用 `event: settlement` 命名事件——三者均会污染 OpenAI 兼容 wire 或被 SDK 视作错误。
- **错误信号分两路**：
  - **流内业务错误**（已开始 streaming 后失败）：发送一帧 `{"error":{...}}`（结构遵循 R4 §5.2.3），随后立即 `data: [DONE]`。Vercel AI SDK 命中其 `'error' in chunk.value` 分支并触发 error 路径。
  - **协议层错误**（streaming 未开始）：按 R4 §5.2.1 / §5.2.2 走 402 + `WWW-Authenticate: Payment` 或 RFC 9457 `application/problem+json`。
- **解析鲁棒性**：接收端 parser 必须容忍 partial frame、multi-line `data:`、空行、注释行（`:`-前缀）；规范应给出最小 normative 测试向量。
- **传输独立性**：本节描述的是**响应 body 与回执通道的 wire 形态**，与 R2 选定的 transport 正交。Body + GET 回退在 HTTP/1.1 / HTTP/2 / HTTP/3 上语义相同；trailer 仅在支持 trailer 的 hop 上即时可达，不可达时回退路径无损补齐。

### 4.4 与第三方 AI SDK 的兼容关系

按 §4.3 改造后：

- **Direct path（Consumer→Provider，P2P 直连）**：BitRouter wire body 是严格 OpenAI v1 chat completions SSE。Consumer 即便直接套用 OpenAI 客户端 SDK，也能正确解析所有 token、`finish_reason`、`[DONE]`；BitRouter-aware 客户端额外读 trailer 或 GET 回执完成结算闭环；**不读 trailer 的客户端**通过 GET 回执仍可拿到 receipt（凭 `challenge.id` 查询）。
- **PGW path（Consumer→PGW→Provider）**：PGW 在内部消化 Provider 的 `Payment-Receipt`，按需向 Consumer 重签 PGW-side receipt（同样走 trailer + GET 双通道）。SSE body 全程是干净的 OpenAI v1。
- **Anthropic-shape 兼容**：不在 R3 范围。需要时由 PGW（或客户端 SDK）做模式翻译，**不污染 BitRouter 内部 wire**。

### 4.5 落地影响

- 规范侧：[`005`](./005-l3-payment.md) §2.5 中"settlement trailer 走 HTTP trailer / fallback GET"现状基本对齐本节方向，需细化字段集合（按 R7 / R8 落定 MPP receipt 结构 + R10 的 `order` 扩展）；并显式规定 SSE body 内**禁止**任何 BitRouter-specific 扩展字段。
- 实现侧：原型 CLI `delta_text` 兼容路径删除；wire 上仅允许 OpenAI 兼容 `choices[].delta.content` shape。Mock upstream 改为 "匿名 SSE OpenAI chunk + 最终 usage 帧 + `[DONE]`"。Provider 端实现 `Payment-Receipt` trailer 写入 + `GET /v1/payments/receipts/{id}` 端点。

### 4.6 已拒绝的备选

- **把 settlement 内嵌进最终 SSE chunk 的 `usage.bitrouter.settlement` 子对象**：早期 R3 草案曾选定此方向，理由是规避 trailer 在 OpenAI SDK 直连时被丢弃。==**已撤回**==。撤回理由：(a) 偏离 MPP `Payment-Receipt` 标准位置，需要每个 PGW 在转发时剥离子对象；(b) 与 Anthropic / OpenRouter 的 `usage` 扩展（`cache_*` / `cost`）共占同一 `looseObject` 扩展点，长期易冲突；(c) 让结算与 LLM-specific schema 强耦合，不能直接迁移到 embeddings / image / audio 等非 chat endpoint；(d) 与 R8 + R10 收敛到 MPP wire 的方向矛盾。引入 GET 回退通道后，trailer "OpenAI SDK 不读" 问题被回退路径覆盖，回到 MPP 标准位置是更优解。**拒绝**。
- **使用自定义 SSE 事件名（`event: data` / `event: usage` / `event: settlement` / `event: error`）**：实证显示主流 AI SDK 均不读 `event:` 字段；自定义事件名让 wire 偏离 OpenAI baseline 且 schema 不匹配的 JSON 会被 SDK 视为错误。**拒绝**。
- **把 settlement 单独编码为 SSE body 内的特殊 JSON 帧（如 `{"settlement": {...}}`）**：会破坏 OpenAI chunk schema 校验、被 SDK 视为错误帧。**拒绝**。
- **丢弃 `data: [DONE]` 哨兵**：删除会破坏部分 consumer。**拒绝**。
- **只保留 trailer，不提供 GET 回退**：浏览器 `fetch` / OpenAI SDK 不读 trailer，Direct path P2P 兼容性不可接受。**拒绝**。
- **只保留 GET 回退，不写 trailer**：BitRouter-aware 客户端被迫多一次 round-trip 才能完成结算确认；trailer 几乎零成本（仅 framing），保留作主通道。**拒绝**。


---

## 5. R4 — 协议错误模型分层（MPP 402 + RFC 9457 Problem Details）

### 5.1 问题

原型实现报告 §8.12 列举至少 9 类协议错误（Provider dial / PGW auth / Voucher invalid / Envelope invalid / Channel insufficient collateral / Chain unavailable / Upstream timeout / Settlement trailer missing / Registry stale snapshot）。早先草案曾提议自定义 `{error:{code,category,retriable,message,details}}` envelope。但收敛 BitRouter 到 MPP 标准（R8/R10）后，**支付类错误已有 MPP 自带的 wire 形态**（[`/protocol/challenges`](https://mpp.dev/protocol/challenges) 的 402 + `WWW-Authenticate: Payment <auth-params>`）；再叠加一份自定义 envelope 会与 MPP 重复且语义冲突。==**应当分层**==：支付/认证类错误用 MPP 标准；非支付类用 IETF [RFC 9457 Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html)，二者皆是工业标准、客户端 / 反代友好、无需自创信封。

### 5.2 建议

在 [`003`](./003-l3-design.md) 新增"§ 协议错误模型"附录，规定：

#### 5.2.1 支付 / 认证类错误（402 + MPP `WWW-Authenticate: Payment`）

凡涉及支付凭证缺失 / 失效 / 不足额 / 凭证不被接受的情形，统一返回 `402 Payment Required` + 标准 MPP challenge header（按 R8 的字段集 `id / realm / method / intent / request / digest / expires / opaque`）；客户端按 MPP 流程提交新 credential 重试。错误归类（如 `challenge.expired` / `voucher.cumulative_regression` / `channel.insufficient_collateral`）通过 MPP `Problem` 扩展或在 challenge 的 `realm` / `intent` 中体现，==**不**==自创独立 envelope。

#### 5.2.2 非支付协议错误（RFC 9457 `application/problem+json`）

凡身份 / 注册 / 上游 / 链 / 传输类错误，返回对应 4xx/5xx HTTP status + `Content-Type: application/problem+json`，body 形如：

```json
{
  "type": "https://bitrouter.ai/errors/registry.snapshot_stale",
  "title": "Registry snapshot is stale",
  "status": 409,
  "detail": "Provider snapshot for did:pkh:... is older than freshness window",
  "instance": "/v1/.../...",
  "code": "registry.snapshot_stale",
  "category": "registry",
  "retriable": true
}
```

- `type` 为稳定 URI（命名空间 `https://bitrouter.ai/errors/<code>`），同时作为该错误码的人读文档锚点。
- `code` 为 `<domain>.<reason>` 字符串枚举（`identity.*` / `registry.*` / `transport.*` / `upstream.*` / `chain.*`，==**不含** `payment.*` / `auth.*`==——后者归 §5.2.1）。
- `category` ∈ `{ identity, registry, transport, upstream, chain }`。
- `retriable` 为 boolean。
- `type` / `code` / `category` / `retriable` 是 RFC 9457 允许的扩展成员。

#### 5.2.3 流内错误（streaming 已开始之后）

按 R3 §4.3 规则，发送一帧 `data: {"error":{"code":"...","category":"...","retriable":<bool>,"message":"..."}}\n\n`，随后 `data: [DONE]`。该帧字段集与 §5.2.2 的 problem+json 一一对应（`code` / `category` / `retriable` / `message` ↔ `detail`），便于客户端用同一份 typed error 渲染。

#### 5.2.4 错误码注册

规范附录给出**最小错误码集合**与**语义边界**；追加错误码须经规范修订流程加入，不得复用已分配 `code`，不预留厂商扩展前缀。

### 5.3 落地影响

- 规范侧：[`003`](./003-l3-design.md) 新增"协议错误模型"附录；[`005`](./005-l3-payment.md) 中所有支付类错误响应换为 MPP 402 形态；其他类换为 problem+json。
- 实现侧：CLI / Provider / PGW 全部对接两类 wire；report §10 的 typed error code 表沿用 §5.2.4 的注册规则。

---

## 6. R5 — 身份字符串编码统一锁定

### 6.1 问题

原型实现报告 §9.1 直接点名："iroh endpoint id 的 hex 表示与 `ed25519:<z-base32>` wire format 不一致" —— 这是实际 e2e 跑通时被 ed25519 wire format 与 iroh 内部表示不一致 trip 到的真实坑。具体来说，iroh 上游（`iroh-base/src/key.rs`）的 `PublicKey::Display` 输出 **hex（64 字符）**，而 `to_z32()` 才输出 **z-base32（52 字符）**——两者并存于同一 SDK，调用方很容易拿错。

[`001-02`](./001-02-terms.md) 当前已规定 `provider_id` / `pgw_id` / `endpoint_id` 的语义层定义，但**没有规定它们在协议消息、URL 路径、CLI 参数、日志输出中的字符串编码**；不同上下文允许不同表示就会反复重蹈这个坑。

需要澄清：本节的"两种编码"说的是「**协议线上唯一允许形式** vs **iroh API 内部默认 hex 形式**」之间的对照与隔离，==**协议线上始终只有一种编码**==，不存在并存。

### 6.2 建议

在 [`001-02`](./001-02-terms.md) 中追加"§ 身份字符串编码"约束：

- **唯一线上形式**：所有协议消息（snapshot、envelope、voucher、settlement、error envelope、PGW link、URL 路径、CLI 参数、结构化日志）中的公钥**唯一**编码为 `<algo>:<z-base32-lower-no-pad>`，其中：
  - **z-base32 字母表锁定为 Zooko 变体**：`ybndrfg8ejkmcpqxot1uwisza345h769`（同 iroh `to_z32()` 实现）。==**不是 RFC 4648 base32**==——后者使用 `a-z2-7` 字母表，与 z-base32 互不兼容。
  - 不带填充（no padding）。
- **algo namespace 锁定**：`<algo>` 取自如下封闭集合（v0/v1 仅 `ed25519` 实际使用，其余为预留字符串，禁止厂商自定义 algo tag）：

  | algo tag | 公钥字节 | z-base32 字符数 | 状态 |
  | --- | --- | --- | --- |
  | `ed25519` | 32 | 52 | v0/v1 唯一启用 |
  | `secp256k1` | 33（compressed） | 53 | 预留 |
  | `secp256r1` | 33（compressed） | 53 | 预留 |
  | `sr25519` | 32 | 52 | 预留 |
  | `bls12_381_g1` | 48 | 77 | 预留 |

  规范修订后追加新算法**必须**进入此表并通过本节修订流程；任何未在表中出现的 `<algo>` 字符串均为非法。
- **iroh 边界规则**：`endpoint_id`（即 iroh `EndpointId`）是 ed25519 公钥的特例。调用 iroh API 时若拿到 `Display` 输出的 hex（64 字符），**必须**在 SDK 边界处通过 `from_z32` / `to_z32` 立即转码为 `ed25519:<z-base32>`，==hex 表示禁止外泄到协议消息、CLI、日志、snapshot==。同样，向 iroh 传 `EndpointId` 时在边界处反向转码。
- **Registry / CLI / 日志**强制使用上述形式；禁止 hex、禁止 RFC 4648 base32、禁止 base64 / base64url、禁止 base58、禁止 multibase 其他前缀（`z` / `f` / `m` 等）、禁止 `did:key:` 形式。
- [`001-02`](./001-02-terms.md) §10 deprecated 表中追加：`hex(endpoint_pubkey)`、RFC 4648 base32、`did:key:z…`、裸 multibase 等同义形式列为禁止。

### 6.3 落地影响

- **clarification**：原型实现已在转码处理；本节只是把"已被实现解决但规范未明示"的契约写入文档。
- CI 增加 R 规则：所有 snapshot / config / fixture / wire fixture 中的身份字符串必须匹配 `^(ed25519|secp256k1|secp256r1|sr25519|bls12_381_g1):[ybndrfg8ejkmcpqxot1uwisza345h769]+$`，且长度等于该 algo 在 §6.2 表中规定的字符数。==**注意**==：z-base32 字母表是 `ybndrfg8ejkmcpqxot1uwisza345h769`，==**不是** RFC 4648 base32 的 `a-z2-7`==。早先草案中给出的正则 `[a-z2-7]{52}` 是错的（会同时漏判合法 z-base32 串与误纳非法字符），本节正式覆盖。
- 规范附录给出 z-base32 字母表与一组 normative 测试向量（覆盖 ed25519 公钥的 hex ↔ z-base32 双向转码、错字母表的拒绝样例）。

### 6.4 备选编码与拒绝理由

以同一把 32-byte ed25519 公钥示例对照（pubkey hex = `f2b9d84b...75e22f`）：

| 编码 | 字符数 | 样例 | 拒绝理由 |
| --- | --- | --- | --- |
| **z-base32 (Zooko)** ← 选定 | 52 | `6kh7o1a1nmxwneynyecnj6yezt54fdhms1zknkmsunh8wedihezo` | — |
| RFC 4648 base32 lower | 52 | `6k45qsysclpuciacaimcj6aixr32fd4lwsxkcklwtc4huidv4ixq` | 与 iroh `to_z32` 不一致；增加一条边界转码 |
| hex lower | 64 | `f2b9d84b…75e22f` | 长 23%，无视觉去歧义；与 iroh `Display` 一致但人眼难辨 |
| base64url no-pad | 43 | `8rnYSxIS30EgAgIYJPgIvHeij4u0rqEpdpi4eiB14i8` | 大小写敏感；URL 中含 `_-` 可读性差；与 iroh 不共生态 |
| base58 (Bitcoin) | 44 | `HLW2TBrbXjpcs6ysX4Sp489spX1xS8iabNoaBtEKUeyL` | 变长（leading-zero 字节算 `1`）；编码无 stdlib 支持 |
| multibase `z<base58btc>` | 45 | `zHLW2TBrb…UeyL` | 不自带 algo 标识，需要外部字段 |
| `did:key:z<multicodec+base58>` | 57 | `did:key:z6Mkvnm…ACLPski` | 强耦合 W3C DID 与 multicodec varint，每语言都需引入 parser；与现有 `<algo>:` 风格不兼容 |

z-base32 vs RFC 4648 base32：长度同为 52 字符，区别只在字母表选择。z-base32 排除视觉易混的 `0/O/l/1/I`，可读性更好；选 z-base32 主要是**与 iroh 上游共生态**——`iroh-base::PublicKey::to_z32()` / `from_z32()` 直接可用，避免在 BitRouter ↔ iroh 边界引入第二条编码转换。代价是每种语言的 BitRouter 实现需要带一份自定义字母表常量（标准库无 z-base32），但实现极简单（一张 32 字符表 + 5-bit 分组），且在测试向量保护下不易写错。

---

## 7. R6 — Provider 必检项显式枚举（统一 Direct + PGW）

### 7.1 问题

原型实现报告 §9.3 指出："PGW path 中，Provider 不能只验证 voucher，还必须验证：(1) 请求是否来自允许的 PGW；(2) envelope 是否绑定了正确 provider；(3) settlement trailer 的 `order_id` 是否与 envelope 对齐。"

这三点目前**散落在** [`004-03`](./004-03-pgw-provider-link.md) 与 [`005`](./005-l3-payment.md) 的不同段落，没有一个集中、规范的"**Provider must-check list**"。互操作实现极容易漏掉其中之一（尤其是 trailer ↔ envelope 的 `order_id` 一致性）。

### 7.2 建议

在 [`004-03`](./004-03-pgw-provider-link.md) 新增"§ Provider 必检项（normative）"列表，==**统一覆盖 Direct path 与 PGW path**==，避免两条路径各维护一份。每条规则标注其作用路径（D=Direct, P=PGW, B=Both）。至少包含：

| # | 规则 | 路径 |
|---|---|---|
| C1 | 入站连接的 endpoint pubkey ∈ Provider snapshot 的 `accepted_pgws` 白名单（`policy=permissioned`）或满足 `policy=open` 的接受流程 | P |
| C2 | `Order-Envelope` header 存在、可解析、签名验证通过、签名 key 与 PGW snapshot 中 `pgw_id` 一致 | P |
| C3 | envelope 中 `provider_id` 等于本 Provider 的 `provider_id` | P |
| C4 | 业务字段（`pricing_ref` / `model` / `intent`）与本 Provider 当前 snapshot 中的 pricing 项匹配；不接受陈旧 pricing snapshot | B |
| C5 | `X-Channel-Voucher` 验证：voucher 通道与对端身份匹配；`nonce` 严格单调递增；`cumulative` 不回退、不超过 collateral | B |
| C6 | Direct path 上 voucher 的 `payer_id` 必须等于发起 QUIC 连接的 endpoint pubkey 对应身份；不允许 envelope（无 envelope 的请求自动归为 Direct path） | D |
| C7 | `Payment-Receipt`（原 settlement trailer）的 `challengeId` / `reference` 必须与本次请求 challenge 的 `id` / 通道 `channelId` 一致；`payload.order.orderId`（PGW path）须等于 envelope 的 `orderId`，Direct path 用 Provider 自生成 `orderId` | B |
| C8 | `Payment-Receipt` 必须由 Provider 自身签名（详见 R7） | B |
| C9 | challenge 的 `digest` 必须等于实际请求 body 的 SHA-256（按 [MPP `/protocol/challenges`](https://mpp.dev/protocol/challenges)）；空 body 用 `e3b0...b855`；mismatch 必须拒绝并以 R4 §5.2.1 形态重发 challenge | B |
| C10 | challenge 的 `expires` 必须解析为绝对时间戳（RFC 3339）且与 Provider 本地时钟差 ≤ snapshot 中允许的时钟漂移；过期 challenge 必须拒绝 | B |
| C11 | credential `payload.order.pricingPolicyHash`（按 R10）必须命中 Provider 当前 snapshot 中某个有效 pricing policy；未命中或已过期必须拒绝并发 `pricing.policy_unknown` problem | P |
| C12 | 实际 token 用量必须 ≤ credential `payload.order.maxInputTokens` / `maxOutputTokens` 上限；超出必须截断并在 receipt 中如实反映，或在 streaming 前直接拒绝并发 `quota.exceeded` problem | B |
| C13 | credential `payload.order.grossQuoteBaseUnits == providerShareBaseUnits + gatewayShareBaseUnits`（基本单位整数严格等式，无浮点）；不满足必须拒绝并发 `order.fee_split_invalid` problem | P |

每条对应一个标准 error code（与 R4 联动）。

### 7.3 落地影响

- **clarification**：原型已实现该列表，本节只是规范化。
- CI 增加 R 规则：测试向量覆盖每条必检项的 violation case。

---

## 8. R7 — `Payment-Receipt` Provider 重签规则显式化

### 8.1 问题

原型实现报告 §9.1 / §9.5 列举的"只有跑通 e2e 才暴露"的真实问题之一："settlement trailer 修改 `order_id` 后必须重新签名"。在 PGW path 中，Provider 收到的 envelope `order_id` 是 PGW 生成、PGW 签名的；Provider 在产出 receipt 时，receipt 内的 `order` 引用必须填回 envelope 的 `orderId`，并由 **Provider 自己**签名（不是简单转发 PGW 签名）。这一规则在 [`005`](./005-l3-payment.md) 现行文本中并未显式写出。

> 注：随 R3 / R8 收敛到 MPP 标准，本节的对象**正式名称为 `Payment-Receipt`**（沿用 [MPP `/protocol/receipts`](https://mpp.dev/protocol/receipts) 命名）；早先草案中的"Settlement Trailer"为同一对象的旧业务名，自本版起在所有规范中替换。其线上承载形态由 R3 规定（HTTP trailer 主通道 + GET 回退），R7 只规定字段集与签名规则。

### 8.2 建议

在 [`005`](./005-l3-payment.md) 关于 `Payment-Receipt` 的小节中显式规定：

- `Payment-Receipt` 字段集严格遵循 [MPP `/protocol/receipts`](https://mpp.dev/protocol/receipts)：`{ challengeId, method, intent, reference, settlement: { amount, currency }, status, timestamp }`，外加 BitRouter `order` 扩展（按 R10：`{ orderId, providerId, pgwId?, model, pricingPolicyHash, maxInputTokens, maxOutputTokens, grossQuoteBaseUnits, providerShareBaseUnits, gatewayShareBaseUnits }`）。
- `challengeId` 必须等于本次请求 challenge 的 `id`；`reference` 在 Tempo session intent 下等于通道 `channelId`（bytes32 hex）。
- `order.orderId` 必须等于本次请求 credential `payload.order.orderId`（PGW path 即 envelope orderId；Direct path 由 Provider 自生成）。
- `settlement.amount` 为 TIP-20 base units integer string（按 R1 / R9）；`settlement.currency` 为支付资产的稳定字符串标识。
- 由 Provider 在每次响应结束时**重新构造、重新签名**整个 receipt：`Payment-Receipt-Sig` header / trailer 携带 base64url(sig)，签名 key 为 Provider 的 `provider_id` 对应 ed25519 私钥，签名输入为 receipt JSON 的 [JCS RFC 8785](https://www.rfc-editor.org/rfc/rfc8785.html) 规范化序列化。
- 显式禁止"转发 PGW 签名"或"复用 envelope / credential 签名"。
- 对应 error code：`receipt.signature_invalid`、`receipt.order_id_mismatch`、`receipt.challenge_id_mismatch`。

### 8.3 落地影响

- 规范侧：[`005`](./005-l3-payment.md) 中所有 "Settlement Trailer" 字样改为 "Payment-Receipt"；字段集对齐 MPP；签名规则固定。
- 实现侧：原型 receipt 字段重排为 MPP 形态；签名替换为对 JCS(receipt JSON) 的 ed25519 签名。
- CI 增加 R 规则：`Payment-Receipt-Sig` 验证、`order.orderId == challenge.payload.order.orderId` 一致性测试向量。

---

## 9. R8 — MPP `Authorization: Payment` / `WWW-Authenticate: Payment` wire 严格对齐

### 9.1 问题

[`005`](./005-l3-payment.md) §2.3 / §2.4 现行写法把 challenge 折叠为 `WWW-Authenticate: Payment challenge="<base64url(JSON)>"` 的单一 auth-param，credential 类似处理。这是 BitRouter 自创的简化形态，**与 MPP 标准 wire 不兼容**：

- MPP 规范（[`/protocol/challenges`](https://mpp.dev/protocol/challenges)）要求 challenge 在 HTTP header 中表示为**多个独立 auth-params**：`id` / `realm` / `method` / `intent` / `request` / `digest` / `expires` / `opaque`，每项 token68 / quoted-string，按 RFC 9110 §11 解析。
- `id` 必须是 HMAC-bound token，输入为 `realm | method | intent | request | expires | digest | opaque` 的 `|`-join（缺省槽用空字符串），HMAC key 为 Provider 私钥派生；这避免任何字段被 in-flight 篡改。
- `request` / `opaque` 内的 JSON 须用 [JCS RFC 8785](https://www.rfc-editor.org/rfc/rfc8785.html) 规范化后 base64url 编码。
- credential（[`/protocol/credentials`](https://mpp.dev/protocol/credentials)）= `Authorization: Payment <base64url(JCS({ challenge, source, payload }))>`；`challenge` 字段必须**逐字节回传**收到的 challenge auth-params 还原对象。

不对齐 MPP wire，会让任何使用 `mppx` SDK / MPP-aware 反代 / MPP gateway 的对接方都需要在边界做格式翻译，违背 [`004-02`](./004-02-payment-protocol.md) 已确立的 "MPP 1:1 对齐" 决定。

### 9.2 建议

在 [`005`](./005-l3-payment.md) §2.3 / §2.4 / §2.5 改写为：

- **Challenge wire**：响应 `402 Payment Required` + `WWW-Authenticate: Payment id="...", realm="...", method="...", intent="...", request="<base64url(JCS(json))>", expires="...", digest="<sha256-hex(body)>", opaque="..."`。所有 auth-params 严格按 [MPP `/protocol/challenges`](https://mpp.dev/protocol/challenges) 字段语义；`id` 由 Provider HMAC 计算并验签。
- **Credential wire**：请求带 `Authorization: Payment <base64url(JCS({ challenge, source, payload }))>`。`challenge` 是 challenge auth-params 还原后的对象（保持 `id` 字段以便 Provider 重新 HMAC 校验）；`source` 为 `did:pkh:eip155:4217:0x...`（按 R9，Tempo session intent）或 `<algo>:<z-base32>`（按 R5）；`payload` 见 R9 / R10。
- **Receipt wire**：按 R7 + [MPP `/protocol/receipts`](https://mpp.dev/protocol/receipts) 形态，trailer / header 使用 `Payment-Receipt`。
- **canonicalization**：所有 JSON 序列化在签名 / hash / `id` 计算路径上一律使用 JCS RFC 8785；规范附录给出 normative 测试向量。
- **error 行为**：challenge 解析失败 / `id` HMAC mismatch / `expires` 过期 / `digest` mismatch — 一律按 R4 §5.2.1 重发 fresh challenge。

### 9.3 落地影响

- [`005`](./005-l3-payment.md) §2.3–§2.5 完整重写；旧 `challenge="<base64url(JSON)>"` 单 auth-param 形态删除。
- 实现侧切到 `mpp` Rust crate / `mppx` TypeScript SDK 的 challenge / credential 解析与构造；不再有自维护 codec。
- CI 加测试向量：合法 / 篡改 challenge、HMAC mismatch、JCS 输入。

---

## 10. R9 — Tempo session voucher 改为 EIP-712 + TIP-20 base units + secp256k1

### 10.1 问题

[`005`](./005-l3-payment.md) 现行 voucher 定义包含 `payer_id` / `cumulative` / `nonce` / `signature` 等字段，签名形态隐含 ed25519。MPP Tempo session（[`/payment-methods/tempo/session`](https://mpp.dev/payment-methods/tempo/session)）规定的 `TempoStreamChannel` 合约链上验签**只接受 EIP-712 typed data + secp256k1 ECDSA**（`ecrecover`），且 `cumulativeAmount` 是 TIP-20 base units `uint256`（不是 BitRouter 自创的浮点 / 有理数）。

不对齐则 voucher 无法直接被链上 `TempoStreamChannel` 验签，需要由 PGW / Provider 引入额外签名翻译层，违背 R1（"链上结算锚定 atomic units"）与 [`004-02`](./004-02-payment-protocol.md) 的 "MPP 1:1 对齐" 决定。

### 10.2 建议

在 [`005`](./005-l3-payment.md) Tempo session 章节锁定：

- voucher 是 EIP-712 typed data。EIP-712 domain：
  - `name = "TempoStreamChannel"`、`version = "1"`
  - `chainId = 4217`（Tempo mainnet）/ `42431`（Tempo testnet）
  - `verifyingContract` = `TempoStreamChannel` 合约地址（mainnet `0x33b9...4f25` / testnet `0xe1c4...a336`，最终值以 [MPP `/payment-methods/tempo/session`](https://mpp.dev/payment-methods/tempo/session) 为准）
- typed data primary type 为 `Voucher`（或 MPP 规范命名）；字段：
  - `channelId: bytes32`
  - `cumulativeAmount: uint256`（TIP-20 base units）
  - `nonce: uint256`
  - `action: string`（∈ `{ "open", "topUp", "voucher", "close" }`）
- signer：Consumer 在 Tempo 上控制的 secp256k1 EOA（DID `did:pkh:eip155:4217:0x...`）；签名为 EIP-712 ECDSA，长度 65 bytes（r/s/v）。
- voucher 在 wire 上承载于 MPP credential `payload`（按 R10），形如 `payload.tempo.voucher = { channelId, cumulativeAmount, nonce, action, signature }`，所有数值字段用 base units integer string。
- Provider / PGW 验签调用 `ecrecover` 还原 signer 地址，与 credential `source` 解出的 EOA 比对一致；不一致按 R4 §5.2.1 拒绝。
- ==**BitRouter Ed25519 keys 角色不变**==：node identity（iroh QUIC TLS）、PGW snapshot 签名 / Provider snapshot 签名、`Payment-Receipt` 签名（R7）、credential 上 BitRouter 自定义字段（R10 的 `order.orderSig`）签名均仍为 ed25519。Tempo voucher 是**唯一**强制 secp256k1 的字段，因为它要被 Tempo 合约链上验签。

### 10.3 落地影响

- [`005`](./005-l3-payment.md) Tempo session 小节按上文锁定字段集；`payer_id` 字段语义改为 `did:pkh:eip155:4217:0x<EOA>`。
- [`004-02`](./004-02-payment-protocol.md) 引用 `mpp` crate 中 Tempo session voucher 类型；本地不再有平行定义。
- [`001-02`](./001-02-terms.md) 增补："Consumer 在 Tempo session intent 下需要持有一把 secp256k1 EOA（Tempo 钱包），与其 BitRouter ed25519 身份解耦。"
- 实现侧：Consumer 端引入 secp256k1 EIP-712 签名能力（viem / ethers / 等价 Rust 库）。
- CI 加测试向量：典型 voucher EIP-712 hash + 签名 + ecrecover 还原。

---

## 11. R10 — Order context 收敛为 MPP credential `payload.order` 扩展（删除独立 `Order-Envelope` header）

### 11.1 问题

[`004-03`](./004-03-pgw-provider-link.md) 现行设计用 HTTP header `Order-Envelope: <base64url(JSON)>` 携带 PGW 给 Provider 的"业务订单"信息（orderId / providerId / pgwId / model / pricing / fee split 等），与 MPP credential 平行存在。这造成：

- 一次请求带两条互相覆盖的"凭证"（MPP `Authorization: Payment` + 自定义 `Order-Envelope`），双签、双校验、双 replay 防护。
- MPP-aware 客户端 / 反代不感知 `Order-Envelope`，可能丢失。
- BitRouter 内部规范在 ["1:1 对齐 MPP" 决议](./004-02-payment-protocol.md) 之后还残留一份独立信封，自相矛盾。

报告 §9.4 推荐把 order context 折叠进 MPP credential 的 `payload`，作为 BitRouter-specific 扩展字段。

### 11.2 建议

在 [`005`](./005-l3-payment.md) / [`004-03`](./004-03-pgw-provider-link.md) 锁定：

- **删除** [`004-03`](./004-03-pgw-provider-link.md) 的 `Order-Envelope` HTTP header。
- MPP credential `payload` 中新增 BitRouter 扩展子对象 `order`：
  ```jsonc
  "payload": {
    "tempo": { "voucher": { /* R9 EIP-712 voucher */ } },
    "order": {
      "orderId":                  "<uuid>",
      "providerId":               "<ed25519:z-base32>",
      "pgwId":                    "<ed25519:z-base32>",   // PGW path 必填；Direct path 省略
      "model":                    "<provider model id>",
      "pricingPolicyHash":        "<sha256-hex of pricing policy doc>",
      "maxInputTokens":           1024,
      "maxOutputTokens":          2048,
      "grossQuoteBaseUnits":      "1500000",              // TIP-20 base units, integer string
      "providerShareBaseUnits":   "1485000",
      "gatewayShareBaseUnits":     "15000",
      "orderSig":                 "<base64url(ed25519-sig over JCS(order without orderSig))>"
    }
  }
  ```
- `orderSig` 由 PGW 用 `pgwId` 对应 ed25519 私钥对 `order` 子对象（不含 `orderSig` 字段）的 JCS 序列化签名。Direct path（无 PGW）下 `payload.order` ==**整体省略**==，Provider 自生成 `orderId` 并在 receipt 中写入；不引入 "Provider 自签 stub" 形态。
- Provider 必检项与 R6 的 C7 / C11 / C12 / C13 联动：`grossQuoteBaseUnits == providerShareBaseUnits + gatewayShareBaseUnits`、`pricingPolicyHash` 须命中当前 snapshot 中有效 pricing、token 限额生效。
- 单一 replay 防护：MPP challenge `id` 已 HMAC-bound 整个请求；`order` 扩展不再单独维护 nonce。

### 11.3 开放点

- `orderSig` 是否冗余？MPP credential 整体已与 challenge 通过 HMAC 绑定，篡改 `payload.order` 任一字段都会让请求与 challenge 解析后的 `request` / `digest` 失配。但保留 `orderSig` 的价值在于：(a) Provider 可在不重算请求 body digest 的情形下单独验证 PGW 对 order 的承诺；(b) 离线审计 / 争议时 `orderSig` 是 PGW 不可抵赖的承诺。**初版保留**，待 v1 评估是否裁剪。

### 11.4 落地影响

- [`004-03`](./004-03-pgw-provider-link.md) 删除 `Order-Envelope` header 章节，全部内容迁入 [`005`](./005-l3-payment.md) "credential payload extensions" 小节。
- 实现侧：PGW 改为构造 MPP credential 时塞入 `payload.order`；Provider 改为从 credential 解析 order，不再读 HTTP header。
- CI 加测试向量：Direct path（无 `payload.order`）、PGW path（含 `orderSig` 验签 + fee split 等式）。

---

## 12. R11 — 协议三段 leg 分层

### 12.1 问题

至此 R1–R10 把 BitRouter 支付 wire 收敛到 MPP 标准。但实践中 BitRouter 协议覆盖**三段不同信任假设、不同性能要求、不同兼容性要求**的 leg，==**不应该用同一套约束统一规定**==：

- **Leg A — Consumer ↔ Provider（Direct path）**：信任最小化，Consumer 可能直接套 `mppx` SDK / OpenAI SDK 调用 Provider；wire 必须严格 MPP 兼容 + 严格 OpenAI v1 SSE 兼容；支付按请求颗粒度。
- **Leg B — Provider ↔ PGW（内部高并发链路）**：长期 B2B 关系，PGW 与 Provider 互相已 onboard；==**LLM stream 高并发**==（同 PGW 可同时持有数百路 stream）；wire 是 BitRouter 内部私有，不需要对外兼容；支付可激进优化。
- **Leg C — PGW ↔ Consumer（外部边界）**：PGW 是面向其客户的网关，Consumer 可以是 OpenAI SDK / Anthropic SDK / x402 client / MPP client / 内部 SaaS 调用；==**协议形态由 PGW 与其客户自行约定**==，BitRouter 不规定。BitRouter 只保证：PGW 收到 Provider 干净 wire 后，有充分能力把它翻译成任意目标形态。

如果把 Leg A 的约束（per-request MPP challenge + receipt trailer）强加到 Leg B，会出现：(a) 高并发下每路 stream 都需独立 challenge round-trip，性能损失；(b) 每帧 SSE body 还要走 receipt trailer，PGW 需在转发到 Leg C 时逐路剥离；(c) Provider↔PGW 的真实信任关系（长期对等方）被低估，过度约束。

### 12.2 建议

在 [`003`](./003-l3-design.md) "§ 协议分层" 顶部新增 **§ 三段 leg 分层** 总纲，规定：

| Leg | 端点 | wire 兼容性约束 | 支付承载约束 |
| --- | --- | --- | --- |
| **A** | Consumer ↔ Provider (Direct) | ==严格 MPP 兼容（R8）== + ==严格 OpenAI v1 SSE 兼容（R3）== | per-request 402 challenge（R8）+ 每路 stream `Payment-Receipt`（R3 + R7） |
| **B** | Provider ↔ PGW | BitRouter 私有 wire；==无对外兼容义务==；可激进优化高并发 | 见 R12：独立 QUIC 控制平面 + 长期渠道累计型 voucher，==LLM stream 不携任何支付字段== |
| **C** | PGW ↔ Consumer | ==**不属于 BitRouter 协议范围**==；由 PGW 与其客户约定（OpenAI / Anthropic / MPP / x402 / 内部 SaaS auth 任选） | 同左 |

- **R3 / R7 适用范围限定 Leg A**；Leg B 的 LLM stream 不写 `Payment-Receipt` trailer、不强制 OpenAI SSE shape（Leg B 仍**建议**保持 OpenAI shape 以便 PGW byte-forward 到 Leg C，但 BitRouter 协议不强制）。
- **R8 / R9 / R10 适用范围限定 Leg A**；Leg B 改用 R12 定义的控制平面 + voucher（==fallback==：若 Leg B 出于某种原因（如初次 onboarding / 紧急 hot-path）没有建立长期控制流，**回退到 Leg A 的 per-request MPP** 形态，保证最小可用基线）。
- **R6 必检项按路径标签 D/P/B 区分**已对此预留；C9 / C10（digest / expires per-request 校验）在 Leg B 长期控制流模式下**可关闭**，因为 PGW 是已认证的长期对等方，replay 防护交由控制流的累计 voucher nonce 单调性保证。
- **PGW 翻译层契约**：BitRouter 规范保证 Provider 在 Leg B 上输出的 LLM stream + 控制平面信息**足够 PGW 翻译成 Leg C 任意目标协议**；PGW 翻译层本身的实现策略不在协议规范内。

### 12.3 落地影响

- 规范侧：[`003`](./003-l3-design.md) 新增 §三段 leg 分层 总纲；R3 / R7 / R8 / R9 / R10 各节顶部加 "applicability: Leg A" 标签；[`004-03`](./004-03-pgw-provider-link.md) 改写为 Leg B 章节并引用 R12。
- 实现侧：Provider 实现按 leg 分两条 code path（dispatch 依据：入站连接对端是否为已 onboard 的 PGW）；CLI 与 mock upstream 在 Leg A 模式默认运行。
- 由此澄清 R10 §11.3 的开放点：Direct path（Leg A）下 `payload.order` 整体省略；Leg B 的 order context 走 R12 的控制平面，不复用 MPP credential 字段。

---

## 13. R12 — Leg B 支付控制平面解耦（独立 QUIC + 长期渠道累计 voucher）

### 13.1 问题

R11 把 Leg B 标定为 "BitRouter 私有、可激进优化"。本节给出具体形态。背景需求：

1. **PGW↔Provider 高并发**：单 PGW 同时持数百路 LLM stream；per-request MPP challenge 的 round-trip 在 hot path 上不可接受。
2. **支付与 LLM 流彻底解耦**：LLM SSE wire body 应可被 PGW byte-forward 到 Leg C，**不应**承载任何 BitRouter-specific 字段（与 R3 在 Leg A 上的目标一致，但 Leg B 上是为了 PGW 转发零拷贝）。
3. **PGW 与 Provider 是长期 B2B 对等方**：信任假设强于 Direct Consumer；可建立长期累计型 channel，按 epoch 批量结算；无需 per-request HMAC challenge。
4. **支付控制流不应阻塞或污染 LLM 流**：包括 buffer / 拥塞 / TLS 握手成本均需隔离。

### 13.2 建议

在 [`004-03`](./004-03-pgw-provider-link.md) 锁定 Leg B 形态：

#### 13.2.1 双连接拓扑

PGW ↔ Provider 之间维持**两条独立 QUIC 连接**（不同 5-tuple，不同 TLS session）：

| 连接 | ALPN | 用途 |
| --- | --- | --- |
| **Data Connection** | `h3`（HTTP/3 over QUIC，按 R2） | 仅承载 LLM API 请求与响应 stream（每路 LLM 调用一条 HTTP/3 bi-stream） |
| **Control Connection** | `bitrouter/payctl/1` | 仅承载支付控制平面（channel 建立 / topUp / voucher / receipt / close / error） |

==**两条连接完全独立**==：建链成本翻倍是有意接受的代价，换取 head-of-line 隔离（LLM 流的拥塞 / 暂停不影响支付握手；反之亦然）、TLS / cert 隔离、可独立 rotate / restart。Control Connection 在 PGW↔Provider link 建立时就建立并==**持久维护**==（idle timeout 关闭后立即重连），不按请求建立。

#### 13.2.2 长期渠道（long-lived channel）

PGW 与 Provider 在 Control Connection 上协商一条长期 BitRouter 内部 payment channel：

- channel 形态借鉴 MPP Tempo session [`/payment-methods/tempo/session`](https://mpp.dev/payment-methods/tempo/session) 但**不对外**：因不需要被 Tempo 链上 `TempoStreamChannel` 合约直接验签，签名形态可改为 BitRouter ed25519（与 R5 / R7 的身份体系一致），不强制 EIP-712 / secp256k1。
- channel 字段：`{ channelId, providerId, pgwId, asset, collateralBaseUnits, openedAt, epochDurationSec }`。collateral 由 PGW 按 R9 形态在 Tempo 链上独立锁定（==链上侧仍走 `TempoStreamChannel` + EIP-712==，链下 BitRouter 内部使用 ed25519 voucher 引用 channelId）。
- channel 生命周期独立于任何 LLM 请求；==**断开 → 重建**==仅是控制流恢复，不影响 LLM 流既有 stream（已发出的 stream 在重建后用新 channel 上的 voucher 计费）。

#### 13.2.3 累计型 voucher（cumulative voucher）

- PGW 周期性（每个 epoch 一次或满阈值时）向 Provider 在 Control Connection 上推送一帧 `payment-voucher`：`{ channelId, cumulativeAmountBaseUnits, nonce, signature }`，==**所有 LLM 请求的累计应付总额单调递增编码于此**==。
- `signature` 为 PGW ed25519 对 JCS({channelId, cumulativeAmount, nonce}) 的签名（**不是** EIP-712；只在 Leg B 链下使用，与 R9 区分）。
- nonce 严格单调；cumulative 不回退、不超过 collateral；与 R6 §C5 一致。
- ==**LLM stream 不携 voucher**==。Provider 在每路 LLM 请求结束时本地累加 `cost(provider_share)` 到一个 in-memory expected_cumulative；定期与 PGW 推送的 voucher 对账（差额 ≤ 阈值即接受）；超阈值或对账失败按 R4 §5.2.2 在 Control Connection 上发 problem+json 错误，并在严重情形下 ==**主动关闭 Data Connection**== 触发 PGW 重建。
- epoch 结束时 PGW 在 Control Connection 上发 `payment-epoch-close` + 最终 voucher，Provider 用最终 voucher 提交链上结算。

#### 13.2.4 LLM stream 上的 `order_ref`

- 每路 LLM 请求在 HTTP/3 请求 header 中带 `BR-Order-Ref: <ulid>`；==这是**唯一**承载于 Data Connection 上的 BitRouter-specific header==。
- `order_ref` 关联 PGW 内部账本（其 fee split / model / pricing policy 在 PGW 内部维护，不需 push 给 Provider）。
- Provider 在每路 stream 结束时（在 Data Connection 上的 LLM HTTP/3 response 完成后）通过 Control Connection 上的 `payment-stream-completed` 帧上报 `{ orderRef, providerShareBaseUnits, usage }`；Provider 不在 Data Connection 的 SSE body 上写任何结算字段。
- ==Provider 不验证 `order_ref` 的内部含义==（不需要 fee split 等式 R6 §C13、不需要 pricing policy hash R6 §C11、不需要 token 限额 R6 §C12）；这些校验完全转移到 PGW 侧。

#### 13.2.5 控制平面 framing

Control Connection 上的帧形态参考 [MPP Ws.serve](https://mpp.dev/sdk/typescript/server/Ws.serve) 的 named-frame 集合（在 HTTP/3 bi-stream 上以 length-prefixed JCS-JSON 帧承载，==**不是** WebSocket==）：

| 帧名 | 方向 | payload |
| --- | --- | --- |
| `channel-open-request` / `channel-open-ack` | PGW → Provider / 反向 | `{ channelId, asset, collateralBaseUnits, openedAt, epochDurationSec }` |
| `payment-voucher` | PGW → Provider | `{ channelId, cumulativeAmountBaseUnits, nonce, signature }` |
| `payment-stream-completed` | Provider → PGW | `{ orderRef, providerShareBaseUnits, usage, completedAt }` |
| `payment-epoch-close` | PGW → Provider | `{ channelId, finalCumulative, finalNonce, signature }` |
| `payment-error` | 双向 | RFC 9457 problem+json |
| `keepalive` | 双向 | `{ ts }`，按 idle timeout 周期发送 |

#### 13.2.6 与 MPP 的关系 / fallback

- Leg B 的 Control Connection ==**不**==使用 MPP `Authorization: Payment` / `WWW-Authenticate: Payment` / `Payment-Receipt` 形态。理由：MPP 是为 per-request HTTP 设计的；Leg B 上把它强加到长连接帧 framing 会引入语义 impedance mismatch。
- ==**fallback**==：若某 Provider 暂未实现 Control Connection（v0 prototype / debugging / 紧急路径），PGW 必须能降级到 Leg A 形态（per-request MPP challenge + receipt）跑通；这是 v0 → v1 演进路径上的过渡保障，==**v1 之后可能弃用**==。

### 13.3 落地影响

- 规范侧：[`004-03`](./004-03-pgw-provider-link.md) 重写为 Leg B 章节，本节内容作主体；新增 ALPN `bitrouter/payctl/1` 注册；[`005`](./005-l3-payment.md) 在 §2 顶部加 applicability 段（"§2 适用 Leg A；Leg B 见 [`004-03`](./004-03-pgw-provider-link.md)"）。
- 实现侧：Provider 与 PGW 各引入 Control Connection 守护逻辑（建链 / keepalive / reconnect / voucher 对账）；现有 Order-Envelope header 路径（已被 R10 删除）的实现替换为 `BR-Order-Ref` header + Control Connection 上报。
- CI 加测试向量：`payment-voucher` JCS + ed25519 签名、Provider 对账逻辑、Control / Data 连接独立性（一边断开另一边继续）。

### 13.4 已拒绝的备选

- **复用 Data Connection 同 QUIC 上独立 bi-stream 承载控制平面**：节省一次握手成本，但 head-of-line 隔离不彻底（同 connection 的 congestion control / 1-RTT crypto 共用），与"支付不应影响 LLM stream"目标矛盾。**拒绝**。
- **复用 LLM 请求所在 stream，仅靠 framing 区分**：与"PGW byte-forward Data Connection 上的 SSE 到 Leg C"目标直接冲突。**拒绝**。
- **per-request MPP（即把 Leg A 形态搬到 Leg B）**：高并发性能不可接受，且 PGW 转发到 Leg C 时仍需剥离 receipt trailer。仅作 R12 §13.2.6 的 fallback 形态，不作主路径。**拒绝主路径**。
- **EIP-712 voucher（与 R9 同形态）**：Leg B voucher 不上链验签，secp256k1 EIP-712 无收益且与 BitRouter ed25519 身份体系割裂。**拒绝**。
- **payment 控制平面用 WebSocket**：HTTP/3 已可承载 length-prefixed framed bi-stream，引入 WebSocket 多一层 framing 与依赖。**拒绝**。

---

## 14. 实施顺序

### 14.1 规范修订与实现替换同步进行

由于无对外兼容负担，本文 12 项修订**不分版本火车批次**，采用以下顺序：

1. **先合 R11**（统领项，纯 wording / 章节框架）——为后续所有 R 项划定 leg 适用范围。R11 一旦合入，R3 / R7 / R8 / R9 / R10 各节顶部即标注 "applicability: Leg A"，避免后续 PR 反复返工。
2. **再合 R5 / R6 / R7 / R1 / R2**（基础设施层：身份 / 必检项 / 重签 / 金额 / transport）——R5 / R6 / R7 是纯 wording 可独立合入；R1 + R2 同批落地，因为两者都要求重签所有 snapshot。
3. **最后两个并行批次，互不阻塞**：
   - **Leg A 收敛批次**：R3 + R4 + R8 + R9 + R10（==**作为同一轮 PR 落地**==，因 challenge / credential / receipt / Tempo voucher / order ext 字段集相互引用）。
   - **Leg B 解耦批次**：R12（独立 ALPN + Control Connection + 累计 voucher）。两批之间无 schema 依赖，可并行推进。

实现仓与规范同步替换；旧 wire 形态在实现层一次性删除，不保留兼容路径。

### 14.2 不进入本轮的事项

下列报告项**不属于协议改进**，不在本草案中提议规范修订，仅在实现仓的工程 backlog 中跟踪：

- §8.1 自实现 JWS / EdDSA → 实现层选择，规范保持算法与 wire format。
- §8.2 chain-mock → 实现层后端替换，[`004-02`](./004-02-payment-protocol.md) 的 MPP 引用已规范。
- §8.3 自写 MPP 层 → 实现策略，与协议规范无关。
- §8.5 PGW ingress TLS → 部署形态，PRD 已规定 HTTPS。
- §8.6 退化身份模型 → 已由 PRD §3.2 明确为 v0 prototype 限定。
- §8.7 中心化 Registry → v1 演进路径，[`003`](./003-l3-design.md) §8.4 已列方向。
- §8.8 channel manager 持久化 → 实现质量。
- §8.9 顺序 load → 实现质量。
- §8.11 shell e2e → 实现工程。


## 15. 已决策项备忘

本草案撰写过程中曾被提出、现已决策落地的事项（避免后续评审重复讨论）：

- **金额表示** — 不采用全局协议 dp，不采用 inline `{value, decimals}`；锚定 token 原生 atomic unit + 报价用有理数（详见 R1）。
- **HTTP/3 ALPN 策略** — 不引入双 ALPN，不保留旧 `bitrouter/p2p/0` 自定义 framing 作 legacy；ALPN 字符串保持，语义直接重定义为 HTTP/3（详见 R2）。
- **流式响应 wire 形态** — 不采用自定义 SSE 事件名（`event: data/usage/settlement/error`）；body 严格对齐 OpenAI v1 chat completions SSE，所有帧使用匿名 `data: <json>`，==SSE body **不携带任何 BitRouter-specific 字段**==（保证 OpenAI / Anthropic / Vercel AI SDK 直连消费的零适配兼容）；保留 `data: [DONE]` 哨兵；流内业务错误用 `data: {"error":{...}}` 帧。
- **结算回执承载** — `Payment-Receipt` 走 HTTP trailer 主通道 + `GET /v1/payments/receipts/{id}` 回退通道（按 R3 + R7）。==**已撤回**==早先草案中 "结算内嵌于最终 chunk 的 `usage.bitrouter.settlement` 子对象" 的方案，撤回理由：(a) 偏离 [MPP `/protocol/receipts`](https://mpp.dev/protocol/receipts) 标准位置；(b) 强迫 PGW 在转发时剥离子对象、与 OpenRouter `usage.cost` / Anthropic `cache_*` 共占 `looseObject` 扩展点长期易冲突；(c) 与 LLM-specific schema 强耦合，不可迁移到 embeddings / image / audio。trailer "OpenAI SDK 不读" 的兼容风险由 GET 回退通道无损覆盖。
- **MPP wire 严格对齐** — challenge / credential 不使用早先草案的 `challenge="<base64url(JSON)>"` 单 auth-param；改用 [MPP `/protocol/challenges`](https://mpp.dev/protocol/challenges) 标准多 auth-param 形态（`id` HMAC-bound、JCS canonicalization）。详见 R8。
- **Tempo voucher 签名形态** — voucher 强制 EIP-712 typed data + secp256k1 ECDSA，TIP-20 base units `uint256`，以满足 `TempoStreamChannel` 合约链上验签。BitRouter ed25519 仍用于 node identity / snapshot / receipt / order 扩展签名。详见 R9。
- **Order context 承载位置** — 删除 [`004-03`](./004-03-pgw-provider-link.md) 的 `Order-Envelope` HTTP header，迁入 MPP credential `payload.order` 扩展，由 PGW 以 ed25519 签 `orderSig`；Direct path（无 PGW）下整体省略 `payload.order`。详见 R10。
- **错误码 namespace** — 不预留厂商扩展前缀；后续追加错误码经规范修订流程加入。本协议无第三方扩展承诺。
- **错误响应分层** — 支付/认证类错误统一走 `402 + WWW-Authenticate: Payment`（MPP 标准）；非支付类走 RFC 9457 `application/problem+json`；流内错误走 `data: {"error":{...}}` 帧。==不==自创独立 BitRouter error envelope。详见 R4。
- **必检项覆盖** — 不分裂 Direct / PGW 两份必检列表；R6 已合并为单一表（C1–C13），按路径标签区分；Leg B 模式下 C9 / C10 / C11 / C12 / C13 由 R12 控制平面 / PGW 内部账本接管，Provider 不再 per-request 校验。
- **协议三段 leg 分层** — 不把所有 leg 统一约束到 Leg A 的 MPP 严格 wire；Consumer↔Provider Direct (Leg A) / Provider↔PGW (Leg B) / PGW↔Consumer (Leg C) 各有独立 wire 与支付契约（详见 R11）。Leg C ==**不在 BitRouter 协议范围**==，由 PGW 与其客户自定。
- **Leg B 支付控制平面** — 不复用 Data Connection、不复用 LLM stream、不在 SSE body 写任何支付字段；==独立 QUIC 连接（ALPN `bitrouter/payctl/1`）+ 长期渠道 + 累计型 voucher + ed25519 签名==；Leg B 上的 voucher 只在 BitRouter 链下使用，不强制 EIP-712（与 R9 区分；EIP-712 仅用于 PGW 在 Tempo 链上锁 collateral 的链上侧）。详见 R12。


## 16. 关联

- 上游：[`bitrouter-p2p-proto/docs/V0_IMPLEMENTATION_REPORT.md`](../../../code/bitrouter/bitrouter-p2p-proto/docs/V0_IMPLEMENTATION_REPORT.md)（v0 实现报告）
- 受影响规范：[`003`](./003-l3-design.md) / [`004-02`](./004-02-payment-protocol.md) / [`004-03`](./004-03-pgw-provider-link.md) / [`005`](./005-l3-payment.md) / [`001-02`](./001-02-terms.md)
- 同级：[`007-01`](./007-01-proto-prd.md)（v0 PRD，定义本次原型的目标基线）
- 下一步产物：每条采纳的 R 项落地为对应规范文档的下一版本（如 `004-02 v0.8` / `003 v0.6`），本草案在所有 R 项关闭后归档。
