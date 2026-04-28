# 007-04 — v2 网络原型实现验收简报

> 状态：**v0.1 — 验收简报**。本文根据 `~/code/bitrouter/bitrouter-p2p-proto` 当前实现、`docs/V2_IMPLEMENTATION_REPORT.md`、`docs/PRD.md`、关键代码路径与本地可运行测试，对 v2 网络原型做一次实现验收。
>
> 原型仓库 HEAD：`a49d811 Add Tempo session lifecycle verification`。

---

## 0. 结论

v2 网络原型可以判定为**已完成第二版 PRD 的主要验收目标**：

- 旧 runtime 主路径中的重复造轮子模块已移除：自写 MPP core、mock chain、JWS voucher、`Order-Envelope`、settlement SSE、HTTP/1 stream shim 均不再是 runtime crate。
- Direct Leg A 已迁移到真实 MPP 402 / credential / `Payment-Receipt`，并使用 upstream MPP Tempo session helper。
- Direct / PGW 数据面已迁移到真实 HTTP/3 semantics over iroh QUIC。
- Leg B 已实现独立 Control Connection：`bitrouter/session/control/0`、JCS control frames、cumulative voucher、epoch close、PGW restart recovery。
- Tempo localnet 已从“Open tx submission”补齐为“session lifecycle”验收：Open receipt、escrow readback、后续 cumulative voucher、close / settle receipt、最终链上状态校验。
- 本地并发与 forced-relay staging 目标已在报告中记录通过：Direct 300 / PGW 1000 local load，staging Direct 100 / PGW 300。

本次复核未重新运行完整 Tempo lifecycle 脚本，因为当前 shell 未配置 `BR_TEMPO_ESCROW_BYTECODE`、`BR_TEMPO_DEPLOYER_PRIVATE_KEY`、`BR_TEMPO_DEPLOYER_ADDRESS`。该项按实现报告中的验收记录接受；本次实际运行了 workspace 与关键 crate / e2e 测试。

---

## 1. 本次复核范围

### 1.1 读取文档

- `docs/V2_IMPLEMENTATION_REPORT.md`
- `docs/PRD.md`
- `docs/TEMPO_LOCALNET_SESSION_REQUIREMENTS.md`

### 1.2 检查代码路径

| 领域 | 代码 / 脚本 |
|---|---|
| Workspace shape | `Cargo.toml` |
| MPP adapter | `crates/bitrouter-mpp-adapter` |
| HTTP/3 over iroh | `crates/bitrouter-h3` |
| Tempo RPC / escrow readback | `crates/bitrouter-tempo/src/lib.rs` |
| Provider Direct Open / voucher validation | `crates/bitrouter-node/src/provider/server.rs` |
| Tempo CLI deploy / inspect / close | `crates/bitrouter-cli/src/cmd/tempo.rs` |
| Full Tempo lifecycle harness | `examples/direct-e2e-tempo-session-lifecycle.sh` |

### 1.3 实际运行的验证

```bash
cargo test --workspace --quiet
cargo fmt --all --check
cargo test -p bitrouter-h3 --test iroh_h3 --quiet
cargo test -p bitrouter-mpp-adapter --quiet
cargo test -p bitrouter-node --test direct_path --quiet
cargo test -p bitrouter-node --test pgw_path --quiet
```

以上命令均通过。

---

## 2. 第二版 PRD 对照验收

| PRD 主题 | 验收结论 | 证据 |
|---|---|---|
| 删除重复造轮子模块 | 通过 | Workspace members 不再包含 `bitrouter-mpp` / `bitrouter-chain-mock`；legacy 代码位于 `tests/fixtures/legacy` |
| 上游 MPP SDK | 通过 | `bitrouter-mpp-adapter` 依赖 upstream `mpp = 0.10.0`，只做 challenge / credential / receipt 映射与校验 glue |
| rational pricing / base units | 通过 | `bitrouter-core` pricing 测试通过；报告称已迁移到 integer ceil arithmetic |
| HTTP/3 transport | 通过 | `bitrouter-h3` 实现 iroh-backed upstream `h3` traits；`cargo test -p bitrouter-h3 --test iroh_h3` 通过 |
| OpenAI-compatible SSE | 通过 | Provider 生成 anonymous SSE chunk、final usage chunk、`data: [DONE]`；Direct / PGW tests 覆盖 |
| `Payment-Receipt` + GET fallback | 通过 | Direct test 覆盖 trailer 与 fallback；Provider receipt record 使用 ed25519 签名 |
| Leg B Data/Control split | 通过 | PGW 数据面只用 `BR-Order-Ref`；控制面使用 `bitrouter/session/control/0` JCS frames |
| Leg B cumulative voucher | 通过 | Provider 验证 nonce / cumulative / collateral monotonicity；PGW restart recovery 测试覆盖 |
| Tempo localnet | 通过 | `docker-compose.tempo.yml` 使用 Tempo dev node；报告记录 `eth_chainId = 0x539` |
| Web3 wallet | 通过 | CLI 支持 Tempo EOA create/import、DID PKH、faucet、balance |
| True Tempo session lifecycle | 通过（按报告） | `examples/direct-e2e-tempo-session-lifecycle.sh` 报告通过；详见 §3 |
| Local concurrency | 通过（按报告） | Direct 300 / 3000 total；PGW 1000 / 10000 total |
| Forced-relay staging | 通过（按报告） | local forced relay 与 Docker two-network forced relay 均通过；staging Direct 100 / PGW 300 |
| Runtime dependency graph | 通过 | `cargo metadata` workspace members 仅含 `bitrouter-mpp-adapter` / `bitrouter-tempo` / `bitrouter-h3` 等新 crate |

---

## 3. Tempo 链上闭环验收

`docs/TEMPO_LOCALNET_SESSION_REQUIREMENTS.md` 要求不能只验证 RPC / faucet / Open submission，而必须验证完整 session lifecycle。当前报告声称已补齐，并且代码路径与脚本逻辑支持该结论。

### 3.1 Open receipt

Provider 在接受 `AcceptedSessionAction::Open` 后：

1. 调用 `eth_sendRawTransaction`。
2. 调用 `wait_for_transaction_receipt(tx_hash, 120, 500ms)`。
3. 如果 receipt 缺失、timeout 或 status=false，返回结构化 `tempo.open_failed`，并在可用时包含 `tx_hash`。

对应代码：`crates/bitrouter-node/src/provider/server.rs` 的 `validate_direct_session_action`。

### 3.2 Escrow readback

Open receipt 成功后，Provider 读取 escrow `channels(bytes32)`，并校验：

- payer
- payee / recipient
- token / asset
- deposit >= suggested deposit
- finalized == false

对应代码：`validate_open_channel_readback` 与 `bitrouter-tempo::TempoRpcClient::escrow_channel`。

### 3.3 后续 voucher update

脚本 `examples/direct-e2e-tempo-session-lifecycle.sh` 在 Open 后再次发起 Direct request，复用 channel，并断言 wallet 中 `last_cumulative` 严格增加，同时校验 response 中有 `payment_receipt`。

### 3.4 Close / settle

CLI 新增：

```bash
bitrouter-cli tempo close-channel
bitrouter-cli tempo inspect-channel
```

`close-channel` 使用 Tempo native transaction 调 escrow `close(bytes32,uint128,bytes)`，等待 receipt，并读取链上 state。生命周期脚本断言：

- close receipt 成功；
- `.finalized == true`；
- `.settled == voucher_cumulative`；
- 输出 settlement tx hash。

### 3.5 仍需注意的边界

- `TempoStreamChannel` bytecode 没有 vendored 到仓库；脚本依赖 `BR_TEMPO_ESCROW_BYTECODE` 指向外部编译产物。
- local Tempo devnet 对随机 faucet wallet 的 fee-token gas accounting 仍有约束；报告中的可复现流程导入 funded devnet EOA。
- 当前 close / settle 是最小 lifecycle 验收，不等同于生产清算策略、争议窗口、批量 settle 或多资产 settlement 设计。

---

## 4. Forced-relay / staging 验收

报告记录以下路径通过：

| 场景 | 结果 |
|---|---|
| local forced relay smoke | `iroh-relay --dev`，Provider addr-file 保留 `home_relay`，清空 `direct_addrs` |
| local staging-target load | Direct 100 / PGW 300 concurrency |
| Docker two-network forced relay smoke | Provider 与 Consumer/PGW 分处不同 Docker network |
| Docker two-network staging-target load | Direct 100 / PGW 300 concurrency，输出 structured metrics |

这满足第二版 PRD 的 prototype staging 目标，但还不是“公网多区域长期运行”验收。

---

## 5. 代码层观察

### 5.1 正向结论

- `bitrouter-tempo` 明确声明只做 wallet/RPC glue，不实现 mock chain 或替代 Tempo protocol。
- `bitrouter-mpp-adapter` 明确声明不实现 alternate MPP state machine。
- `bitrouter-h3` 独立承担 HTTP/3 adapter，不再恢复旧 HTTP/1 stream shim。
- Legacy 搜索命中主要落在 `tests/fixtures/legacy` 和旧文档快照。

### 5.2 小问题

- `crates/bitrouter-cli/src/cmd/snapshot.rs` 仍有 `Phase G stub` 注释。它看起来不是本次 v2 主路径 blocker，但后续如果 CLI snapshot prepare 要进入真实发布路径，需要补齐或删除 stub 语义。
- 原型仓库中的 `docs/V0_IMPLEMENTATION_REPORT.md` 与 `docs/IMPLEMENTATION_STATUS.md` 仍描述第一版实现。现在应将 `docs/V2_IMPLEMENTATION_REPORT.md` 视为当前状态源，旧文档需要加 obsolete 标记或改名归档，避免误导。

---

## 6. 验收结论

v2 网络原型已从“协议闭环 mock 原型”推进到“接近真实网络的工程原型”：

1. Runtime 主路径不再依赖第一版自研 mock chain / MPP core / JWS voucher / custom framing。
2. Leg A 使用真实 MPP + Tempo session helper + HTTP/3 over iroh + OpenAI-compatible SSE + `Payment-Receipt`。
3. Leg B 使用 Data/Control split 与 cumulative voucher，并覆盖 PGW restart recovery。
4. Tempo localnet 不只验证 Open submission，已经按报告补齐 Open receipt、channel readback、voucher update、close / settle final state。
5. 并发与 forced-relay staging 达到第二版 PRD 的 prototype target。

因此，本简报建议将 v2 prototype 标记为：**第二版 PRD 验收通过；进入下一阶段前，应先清理旧状态文档，并决定 Tempo escrow artifact / funded localnet EOA 的可复现交付方式。**
