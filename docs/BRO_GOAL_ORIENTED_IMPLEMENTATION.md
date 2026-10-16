# BRO Goal Workflow Implementation

目标：完整实现 [PRD v0.4](BRO_GOAL_ORIENTED_PRD.md)，遵循 [implementation spec](BRO_GOAL_ORIENTED_IMPLEMENTATION_SPEC.md)。开始日期：2026-10-12。

本文保留原 MVP 的验收记录。2026-10-15 开始的执行重构与 Goal IPC v5，见
[BRO 重构实施记录](BRO_ORCHESTRATOR_REFACTOR_IMPLEMENTATION.md)；下列旧 head 的
v4 和闭环证据不能证明重构后的共同执行路径已完成。

## 基线与进度

- 实际起点：`9b9a176288896809040a6fce11c92d146a4f5909`。
- 本地分支：`codex/bro-goal-workflow-mvp`；用户在 2026-10-14 明确请求 PR to main，授权本次 commit/push/创建PR；未授权 merge、release 或部署。
- 初始修改：`docs/README.md` 索引，以及未跟踪的 PRD 和 implementation spec，均为本次产品讨论的文档。没有初始源码修改。
- Implementation spec 已补至 v0.2；文档进展不计为 MVP 完成。
- 当前阶段：本地 MVP 验收及原功能 head 的跨平台 CI 已通过。当前文件保留最终摘要；逐轮记录和原始证据已迁往独立、固定提交的归档。清理没有改变功能代码、测试或已记录结果。

| 阶段 | 状态 | 已取得证据 / 剩余工作 |
| --- | --- | --- |
| S0 | 本地MVP审计完成 | checkout/AGENTS/spec/入口review及全部62个G/A/F条目有对应来源和证据范围，历史中点结论由最终对账替代。 |
| S1 | 本地MVP已验证 | Goal协议v4、事务/CAS、原身份、权限版本、旧outbox拒绝、冷历史和receipt归档由最终全仓检查覆盖。 |
| S2 | 真实闭环已验证 | R16两条实际并行ACP分支、独立执行保持、版本替换、实际检查依赖传入、集成和交付；资源/能力反例有确定性进程测试。外部未知效果不自动恢复。 |
| S3 | 本地MVP已验证 | 实际中间Enter和54+18输入记录、持久控制/修改/澄清与暂停编辑测试；typed原判定错误单列，当前全部升级推理。 |
| S4 | 本地MVP已验证 | R16实际检查、8条件语义评估和默认自主完成；预算、未知费用预留、完成竞态与有界恢复有最终检查及失败案例证据。未声称完整外部费用硬上限。 |
| S5 | 本地MVP已验证 | 实际PTY覆盖草稿/输入/编辑器/导航/resize/suspend/权限/只读/detach；R16实际完成后重连保持身份且不新增工作。非人体工学研究或全平台认证。 |
| S6 | 本地MVP迁移已验证 | bare/code默认Goals、移除agent参数、保留native/ACP入口；CLI/skill/manifests一致，最终native/session回归通过。既有opt-in跳过和vendor timeout保留。 |
| S7 | 本地验收完成 | 真实服务、输入评测、62项对账和全部最终本地检查通过；原功能 head 的 hosted CI 已通过。生产及全部品牌认证不在该证据范围。 |

## 最终验收与复现

- [最终 62 项验收对账](evidence/bro-goal-workflow/2026-10-15-final-acceptance.md)。
- [最终证据索引与范围](evidence/bro-goal-workflow/README.md)。
- [可移植复现步骤及原始输入](evidence/bro-goal-workflow/REPRODUCE.md)。
- 原功能提交：`4772d9e09deb9c69f539e97d2b24dda9ca770e0c`；其 Windows、macOS、Linux 测试与 Clippy、其余检查已通过：[CI run](https://github.com/bitrouter/bitrouter/actions/runs/38737081691)。后续提交的 CI 单独核对，不从旧 run 推导。
- 本地最终完整检查：4,675 passed / 24 原有 opt-in skipped；doctests 6 passed / 1 原有 ignored。原 source binding 保持不变，不改称清理后重新运行。
- R16 实际 Claude ACP 闭环：两条分支并行、中间 Enter 保存要求2、独立分支保持、旧 text 协调后替换、检查依赖实际传入、最终组合测试与自主完成。实际 worker provider/model 身份仍未报告。
- 输入评测：54 corpus + 18 sequence records，无禁止的持久效果；typed 语义错误单列，当前全部升级推理。q03/q07 为人工确认，其余标注仍是 draft。
- D260 与 D262 的 Windows 修正仅涉及已有 test helper 条件和 fixture 路径；两份 applicability 摘要保留。历史失败、超时、原生 response-format 失败及完成状态纠正仍在归档中。

## 独立归档

[完整 D01-D262 决策与逐轮记录](https://github.com/bitrouter/bitrouter/blob/382c2b58c5c25b02805175a33d60707dc4227b46/docs/BRO_GOAL_ORIENTED_IMPLEMENTATION.md)和全部 890 份原始证据按原字节保留于独立分支 `codex/pr981-evidence-archive-20261015`。

固定提交：[`382c2b58c5c25b02805175a33d60707dc4227b46`](https://github.com/bitrouter/bitrouter/tree/382c2b58c5c25b02805175a33d60707dc4227b46)；[归档定位与校验值](evidence/bro-goal-workflow/archive.json)。归档包含891个原始文件、64181265字节及逐文件SHA-256；原始失败不转换为成功，未发布的私有内容没有加入公共归档。

## 证据限制

已证明的是受控本地 MVP、最终功能提交的 CI 和记录的真实 campaign。未知外部效果继续阻塞；未声称全局 OS sandbox、完整外部账单硬上限、全部 harness/provider 认证或生产可用性。配置/route 确认不等于实际工具使用或物理模型身份。最终交付、人工验收、合并、发布和部署仍是各自独立的条件。
