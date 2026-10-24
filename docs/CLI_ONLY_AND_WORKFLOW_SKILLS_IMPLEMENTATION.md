# CLI-only 与 skills 目录：实现记录

状态：本轮已实现，功能回归已复验；最终 macOS 全量与 doctests 启动阻塞，hosted CI 单独报告。当前行为以
[spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) 与 [CLI](CLI.md#skills) 为准。

## 原 G00–G11

原执行完成了第一方 TUI 删除、行式 onboarding、后台 readiness、connected agents、
单次 CLI 控制、MCP CRUD，以及当时要求的安装库和自动 skills 选择。
这些旧安装/自动选择要求已被用户最新指令取代，不作为当前功能要求。

完整阶段记录、原失败与回归及上一轮精简验证保存在
[不可变实现记录](https://github.com/bitrouter/bitrouter/blob/c122b437e1ceae64b07151cf8577d0af0214a0bd/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md) 与
[原 G11](https://github.com/bitrouter/bitrouter/blob/c122b437e1ceae64b07151cf8577d0af0214a0bd/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
历史结果对应各自源码，不代表本轮通过。

## 本轮收敛

- 删除 app skills 的 Git acquisition、installer、alias/installed.json 发布与磁盘
  version snapshots。旧用户数据留在磁盘，不自动删除或继续作为安装库使用。
- CLI 直接 list/show/init/read/write 本地目录；共享格式、显式 root/作用域、目录身份、
  UTF-8 上限、路径约束和原子写入；不执行脚本或启动执行 owner。
- SkillDirectory 从现有目录生成只读 manifest。LocalSkill 是原路径/内容指纹，
  不生成持久版本。原 caller/workspace 与文件原始字节每次使用前复核。
- Core 只选显式 includes，保留原 Turn、MCP 许可与内容批准、支持文件、模型预算、
  checkpoint 与恢复。去掉 skill relevance 请求、概率、decision policy 和新付费记录。
- Native context 使用完整保守视图，不再请求语义分类/压缩选择；显式 evidence 工具
  与原始证据保留。SDK 模型路由的 decision runtime 和 provider Decisions API 独立保留。
- ACP supervisor 只交付显式本地 skill，不再接入 decision runtime。两个 adapter 的
  载体、审批、取消、超限、错误、uncertain 和原 ledger 保留。
- Onboarding 可选资源仅 MCP；删除安装菜单/flags，保留部分失败和已保存进度。

## 验证与剩余范围

当前验证结果统一写入 [G11 后续收敛验收](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
安装与自动选择测试随被移除行为退役；新的目录/字节变更/零 backend 调用测试验证
替代行为，未把它们改为 ignored。真实 provider/vendor adapter、远端互操作、hosted CI
与跨平台执行仍分别报告。本轮不合并、发布或部署。
