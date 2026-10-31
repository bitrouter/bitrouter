# CLI-only、目录 skills 与显式编排

状态：已实现，验证资格见 G11；本文件按用户最新要求取代旧版安装与自动选择设计。
旧完整契约见[不可变历史](https://github.com/bitrouter/bitrouter/blob/c122b437e1ceae64b07151cf8577d0af0214a0bd/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md)。

## 1. 本轮范围

保留无 TUI 的 onboarding、后台 daemon、ACP/MCP、外部 agent 管理和 native
无界面执行引擎。本轮不删除 native 模型/工具循环、子 agent、Thread/Turn、审批、
预算、持久化和恢复；SDK 模型路由及 public Decisions 接口仍有独立消费者。

移除 BRO 的 skills 安装/Git 获取/更新/卸载、安装 alias、installed.json 与历史
bundle 版本库。获取和第三方安装由 `npx skills` 负责；BRO 直接操作现有目录。
移除 native skill、native 上下文和 supervised ACP skill 的 decision backend
自动调用。用户或外部 agent 明确选择资源，不新增 workflow 分类器或选择引擎。

## 2. CLI 与目录

```text
bro skills list [--workspace PATH | --global | --root PATH]
bro skills show NAME_OR_DIRECTORY_ID [scope flags]
bro skills read NAME_OR_DIRECTORY_ID [--file RELATIVE_PATH] [scope flags]
bro skills write NAME_OR_DIRECTORY_ID [--file RELATIVE_PATH] --from FILE|- [scope flags]
bro skills init NAME [-o PATH]
bro skills explain --thread THREAD --turn TURN [--config PATH]
bro skills explain --managed-session NAME --request-id KEY [--config PATH]
```

- 默认从当前目录、显式 workspace 的 conventional skills 布局发现；global 从
  `.agents`、`.claude`、`.codex` 与显式 CODEX_HOME 的目录发现。root 为明确目录。
- 三个 scope flags 互斥。不向父 Git root 自动写入或安装；同名需准确目录身份
  或更窄 root，不能静默替换。ID 来自 canonical 目录路径，不是安装 UUID。
- list/show 展示路径和 frontmatter 可用性，read/write 默认文件为 SKILL.md。
- write 从文件或 stdin 读取至多 256 KiB UTF-8，通过原 publication lock/CAS
  和原子 writer 更新指定文件。根文件须满足共享格式与目录名约束；支持文件的
  父目录须已存在。不执行脚本，不修改 daemon 配置，不启动 agent 或 MCP。
- 拒绝路径越界和文件 symlink；不把缓存当可编辑本地目录。init 只创建 source。
- 旧 `.bitrouter/skills` 数据留在磁盘，既不删除也不继续作为版本库发现。
  用户可通过外部工具将需要的 source 放入所选正常目录。

## 3. 显式 native 绑定

已有 `--skill project:ID|global:ID|mcp:SERVER:URI` 与 `--exclude-skill` 保留。
include 是必需项，exclude 禁止使用；矛盾或缺失在实际执行前失败。
没有 include 时不自动选中任何 skill，即使 App 配置了 decision_model。

Core 仍在原 agent Turn checkpoint 冻结目录/来源身份、完整文件清单、内容摘要、
约束、读取结果和批准；恢复及 cold explain 不接受缺少显式 include 的已选来源。
最多四个显式 skill，根指令封装合计至多 64 KiB，且受
原模型/上下文预算约束。不截断必需指令来通过准入。

LocalSkill 表示已有目录的内容指纹；没有磁盘版本快照。每次实际读取核对原
caller/workspace、目录范围、相对路径、字节数和 SHA-256。目录编辑、删除或
内容变化可以使当前 Turn 拒绝继续；新的 Turn 才可绑定新内容。
`skill_read` 只读取当前已选定来源清单内文件，返回校验后的 UTF-8 user 数据，
不执行脚本、授予工具或激活嵌套 skill。Cold explain 只投影原数据库事实。

## 4. MCP 边界

Skills over MCP 是发现与读取协议；本地目录写入不转成 MCP 写入或 Git 安装。
保留 gateway 的技能元数据、list/get、资源读取、分页、来源和错误转发。
Native host 仍先检查独立来源许可，再对确切 skill entry 请求内容批准，最后
按需读取和验证原始字节。普通命令批准不代替 skill 批准；父批准不代替子批准。

完整 manifest、server+URI 身份、当前 capability、caller/workspace 权限、缓存
校验与嵌套批准边界不因删除本地版本库而放宽。MCP manifest/digest 是协议完整性
事实，不等于本地安装版本库。固定规范与 M01–M10 原始契约见
[MCP 附件](https://github.com/bitrouter/bitrouter/blob/80550c9b2079a6e23ceeeb6babf922a77e99b5bb/docs/CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md)，其中自动选择描述以本文件为准。

## 5. ACP 交付

Supervised ACP 仅对显式 local includes 准备原始 input 的根指令：Codex Text，
Claude 需 negotiated embeddedContext。原 supervisor ledger 确认 prepared receipt
后才能 dispatch，保留取消、失败、uncertain、重启只读检查与原 Turn 身份。
不调用 decision backend，不增加原生 skill 激活、支持文件访问或 MCP-origin
交付保证；adapter 不具备能力或必需内容超限时拒绝，不偷偷降级来源。

## 6. 自动决策移除与保留项

Native Core 不再发起 skill relevance/context assessment 请求，不新增相应付费
intent/outcome。原 context/evidence、工具配对、protected groups、子任务共享和
显式 search/recall/publish/extract 保留。发布摘要不会自动使它进入压缩后的视图；
默认编译完整可用证据，超出硬上限失败，不自动省略或截断。
Native policy run 不再生成 workflow 分类标签；依赖语义标签的路由不会触发额外
分类请求，仍可使用已配置的保守默认。原路由计划和 learn 归因记录保留。

SDK decision_model 配置、typed executor、模型路由评估、provider Decisions API
及已有计量路径保留。它们不属于本轮退役的 native/ACP 自动选择。
历史 decision/checkpoint 类型只为原证据的检查与恢复验证保留；未确认执行不重试。
旧安装来源不被静默重解释成目录来源，旧数据不会因一次查询而迁移或删除。
旧安装 ID 和含 `managed` 安装来源的技能 checkpoint 不自动迁移，不能作为新目录
绑定恢复；本轮恢复验证覆盖新目录/MCP 绑定和保留的 native 执行路径。

## 7. Onboarding 与分发

保留 provider、agent、Finish 和有限行式交互；可选资源只有 MCP 配置。
移除 --install-skill/--skill-ref/安装 scope flags 与 Install skill 菜单。
MCP 部分失败仍保留已保存 Router 配置并阻止 Finish；setup 不授予来源许可。
CLI reference、AGENTS、分发主 skill/引用和三个 plugin manifests 同步。

## 8. 验收

1. 实际 CLI 在外部准备的目录上完成 list/show/read/write，无安装索引/版本库。
2. 同名可区分；非法 frontmatter 写入不破坏原文；路径/符号链接不能越界。
3. 原安装 commands 和 setup flags 被拒绝，不运行 Git 或脚本。
4. 显式 local 根/支持文件进入原 Core 模型调用；修改原字节被拒绝，cold explain
   不读取源文件、不启动 owner，原审批/效果/恢复边界继续有效。
5. 显式 MCP 元数据→批准→读取→模型链及 revoked/denied/stale/child 场景保留。
6. 配置真实本地 decision HTTP fixture 后，native/ACP 无语义请求；SDK 路由
   的独立评估测试仍通过。不得只把自动选择默认关闭而留下可触发旁路。
7. 两个独立 ACP parsers 保留实际载体、权限、取消、超限、错误和重启证据。
8. retirement 清单只涵盖移除的安装/自动选择行为；替换测试验证新的产品边界。
9. 最终源码上运行完整 nextest、严格 Clippy、fmt、doctests 与 dist-helper check。
   本地 fixtures、hosted CI、真实 adapter/provider 和跨平台资格分别报告。
