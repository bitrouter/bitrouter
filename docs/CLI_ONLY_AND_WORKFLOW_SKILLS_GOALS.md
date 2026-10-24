# CLI-only 与 skills：阶段索引

原 G00–G11 的执行卡片已完成其当时约定范围，包含现已退役的安装器和自动选择。
原阶段提示词不再是当前待办，也不构成自动启动 goal 的授权。

- [当前行为契约](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md)
- [当前实现摘要](CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md)
- [当前验证与原证据边界](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)
- [原完整阶段卡片及提示词](https://github.com/bitrouter/bitrouter/blob/c122b437e1ceae64b07151cf8577d0af0214a0bd/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md)

## G09 — 显式 skills 与 MCP

当前不执行 workflow 自动选择。保留目录指纹、明确选择、MCP 来源、批准与按需读取。
旧 G09 的安装版本绑定与 decision 选择记录属于历史实现。

## G11 — 集成审计与本地交付

每次收敛在最终源码上验证目录 CLI、显式 native/ACP 交付、MCP 负例和原执行边界，
再运行完整 nextest、Clippy、fmt、doctests 和生成物检查。历史通过数不能代替当前证据。
保留无关工作区编辑，CLI/配置/wiring 变化与分发 skill/manifests 同步。
本地、hosted CI、真实服务/adapter 与各 OS 资格分别报告。
