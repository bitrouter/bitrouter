# S6 entry migration review

2026-10-13。范围：既有 `entry_migration_review` agent 的只读源码 review；本轮未改变生产 CLI、运行 daemon 或测试。本记录是迁移盘点，不是入口验收证据。

实现前提仍为 implementation spec §7：工作闭环之后再切换默认入口。`goal_code::run` 已有 RPC 前端源码，但尚无当前版本类型检查、真实 ACP/provider 闭环或 PTY 通过证据。

## 入口与参数

| 当前消费者 | S6 处理 |
| --- | --- |
| 裸 `bro` / `bro code` | 同一 Goals 前端，打开目录不创建 Goal；不以旧 `chat.agent` 为默认入口条件 |
| `code <agent>`、隐藏 `chat/tui` | 移除或解析时明确拒绝；没有字符串重解释和单会话 alias 绕行 |
| CodeArgs 的 thread/load/resume/model/model-policy/output/check/read-only/direct/base-url/no-start/turn-timeout | 从 Goal 入口移除，保持 run/launch/native API 原有含义；不静默转成 Goal 设置 |
| `--workspace` / `--config` | 可明确保留；workspace 仅供新 Goal，不修改已有 Goal 的工作区 |
| CodeArgs `--socket` | 旧语义为只读 ops；不直接升级为可提交 Goal UI。内部 frontend seam 与公开 CLI 授权分开 |
| 可选 `--goal-id` | 需要时显式新增；不复用 `--thread-id`。最小迁移可以不暴露 |
| remote code | owner-only Goal service 尚不支持；明确拒绝，不退回旧 dashboard 或启动本地 Goal |
| 非 TTY | 进入 Goal UI 前明确拒绝，提示 headless run；不调用旧 ACP plain renderer |

主要来源：`apps/bitrouter/src/main.rs` 的 CodeArgs、Command variants、terminal/logging 分类、本地/remote dispatch、code handler 与 parser tests；`apps/bitrouter/src/onboarding.rs` 的 entry、start_default_chat、finish_launch、interactive_after 和 build_snippet。

Onboarding 的 credentials/config/init serve/exit 合同保留；`--after launch` 接 Goals。旧 chat 配置保留可读，不删除或自动转换成 Goal。旧 `bro code codex --base-url ...` snippet 改为有效 native `bro codex --base-url ...` 示例。

## 必须保留的合同

- `bro claude/codex/...` 与 `bro launch <agent>` 的真实 TTY、逐次 config overlays、退出码和计量。
- `bro run <agent>` 的 prompt/schema、native load/resume、headless permission、background 与 supervisor 身份。
- `bro acp serve <agent>` 的纯 ACP stdout、stderr 诊断、native harness session、cancel/lifecycle 与权限转发。
- `agents` 管理、native local v17/HTTP v2、ThreadService、task run/managed、native store/migration 和既有记录。
- ACP 按协议能力协商，无品牌 allowlist，不因能力不足改用 native executor 冒充同一路径。

## 测试迁移

| 来源 | 要求 |
| --- | --- |
| main.rs parser tests | 新 flags、旧 agent positional/flags/aliases 拒绝、local/remote/TTY 语义 |
| tests/onboarding.rs | 裸入口与 after-launch 新合同、旧 config 保留、不因目录观察派发 ACP/model |
| tests/native_agent_tui.rs | 原 `code --thread-id/--model-policy` 不再有效；native 状态/权限/持久化断言移至保留 API，终端行为由 Goal PTY 覆盖 |
| tests/code_tui_pty.rs 及 evolution 场景 | 替换退出的单会话产品场景；不能只是改命令后保留不适用的 palette/FIFO 断言 |
| code_tui_pty/background.rs | 保留有效 headless/supervisor 合同，不随旧 UI 整份删除或 ignore |
| tests/acp.rs | 被删除 chat alias 明确拒绝，机器输出和协议合同由 run/serve 继续验证 |
| 新 Goal PTY | 真实 binary 的 Goals 入口、无 cold-read 效果、两分支/运行中输入、右侧只读、草稿/焦点、resize/恢复和 detach 不 stop |

既有连接 timeout 的来源未证实；不得通过 skip/ignore、放宽断言或删除仍有效场景“修复”验证。

## 锁步文档

迁移时同步 `skills/bitrouter/SKILL.md` 与 references 的 cli、harness-codex、harness-claude-code、diagnose、decision-native、sessions；三 plugin manifests 的描述；README、内部 CLI/DEVELOPMENT/README。标记旧 Conversation/ACP TUI 产品入口规格被新 Goal spec 替代，保留底层合同。历史 evidence 不改写成新版本证明。

当前 default dispatch/CodeArgs/onboarding 未改。下一步先取得可工作闭环证明，再按上述消费者一次明确迁移；接线本身不算 S6/S7 验收。
