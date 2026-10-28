# CLI-only：MCP Skills 协议与 Host 验收

状态：固定协议契约保留；本轮功能回归已复验，最终全量 macOS 验证启动阻塞。原 G09–G11 的
通过记录只对应历史源码，当前结果见 [G11 验收](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
行为边界以 [当前 spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md#4-mcp-边界) 为准。

## 1. 规范与角色

以官方 [Skills overview](https://modelcontextprotocol.io/extensions/skills/overview)
和 [stable specification 固定版本](https://github.com/modelcontextprotocol/ext-skills/blob/c1aa87e4c381c82b99aa37471d0eae4ab50f9985/specification/stable/skills.mdx)
为依据；后者是本阶段的规范正文，基于 MCP `2026-07-28` 或后续兼容版本。
固定 commit 为 `c1aa87e4c381c82b99aa37471d0eae4ab50f9985`，原文 SHA-256 为
`6e33a33762783a02dc4c13c889dda5b8d7c525649012cf77da12d80c04ce6851`。
验收同时记录实际协商版本；跟随 `main` 的链接仅用于发现修订，不作为可复现的
验收基线。升级规范需重新评估差异，不能沿用旧测试推定兼容。

扩展负责 MCP 上的发现、读取和 host 加载约束，不规定本地包管理器的安装事务。
下表分别验收，不能用任一角色的成功替代另一角色。

| 角色 | 本项目范围 | 完成证据 |
| --- | --- | --- |
| 本地目录 | 发现、读写已有目录，安装获取交给外部工具 | 独立于 MCP wire/host，不提供 Git 安装或版本库 |
| MCP gateway | SDK 向下游提供协议，向已配置上游转发、聚合、隔离来源 | A27 / M01–M03；不把转发成功称为激活 |
| Native MCP skill host | G09 daemon/Harness 从获许可上游按需加载并交给 native agent | A28 / M04–M10，明确选择后的执行证据 |
| 外部 ACP/launcher host | G10 按 adapter 能力交付，保留原执行方的权限和加载责任 | A20 与 A28 的外部交付切片；逐 adapter 验证或报告 unsupported |
| 本地 origin MCP server | 继续遵守 [OSS 边界](OSS_MCP_BOUNDARY_SPEC.md)，不恢复本地技能库发布 | 不新增 `bro mcp serve --backend skills` 或隐式 origin |

当前只加载明确选定的本地目录或获许可 MCP 条目。MCP caller/workspace
来源许可先于元数据与正文读取；配置工具、连接成功或 server instructions
提及一个 skill 均不授予该许可。扩展不增加安装或本地资源写入方法。

### 1.1 规范要求与产品选择

| 性质 | 本阶段契约 |
| --- | --- |
| 扩展要求 | 当前连接能力门控、完整 entry、直接 URI lookup、原始字节与完整 frontmatter 校验、来源/manifest 约束，以及规定的批准和缓存责任 |
| 扩展要求 | host 支持单 skill 至少 512 文件/16 MiB 的清单；清单处理能力与每次上下文/文件读取预算分别验收 |
| 规范允许、产品选定 | 不实现可选 directoryRead；native host 第一版拒绝 dynamic；缓存每次命中重验字节；忽略远端 allowed-tools 授权字段 |
| BitRouter 产品策略 | 已有本地目录可进入候选，但仅显式 includes 加载；MCP 来源另需操作者明确允许；Turn 内 manifest 固定，刷新只影响后续安全边界 |
| BitRouter 初始预算 | 最多 4 个选中 skills、初始指令合计 64 KiB，再取现有 host/context/model 的更小上限；这些数值不是 MCP 的协议上限 |

服务器的 512 文件/16 MiB 建议值不是所有合法 entry 的硬性协议上限。更大条目
超出本产品实现预算时须报告预算限制，不据此声称上游违反规范。目录读取不实现
时不声明能力；gateway 的 direct relay 与 aggregate 支持范围分别报告。

名称按 [Agent Skills 格式规范](https://agentskills.io/specification) 的 Unicode
小写字母/数字及单连字符校验，1–64 按字符数计算；目录解析、native host 和 gateway
共用规则。URI 父目录在单次 percent 解码后与名称比较，保留原 URI 与来源身份，
不把 Unicode 名称当作 host 分配的 MCP server label，也不进行 DNS 解析。

共享 URI/IRI 校验保留 rootless、authority 与绝对路径形式，不做 URL 归一化。
`SKILL.md` 和父目录约束检查路径部分；query/fragment 仍是完整资源身份的一部分，
不因目录计算而从 entry、批准、RPC 或缓存绑定中删去。相对路径只解析到原
清单；编码或参数造成歧义时要求精确 URI，不能默认挑选某个版本。

## 2. 实现与证据边界

Gateway 与 native host 源码保留独立验证；自动 workflow 选择与安装器现已退役。
原起点、修复与历史资格见[不可变附件](https://github.com/bitrouter/bitrouter/blob/c122b437e1ceae64b07151cf8577d0af0214a0bd/docs/CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md)。
当前实际结果统一见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。

## 3. Gateway：A27

| ID | 必须成立 | 必需本地证据 |
| --- | --- | --- |
| M01 | 保存当前连接的上游能力；仅在声明 Resources 与 Skills extension 后调用 skills/list/get；目录方法另需 directoryRead=true。重连不得沿用旧连接的能力 | Cargo-built 上游记录实际请求：未声明/声明缺失/false 时零越权方法；支持时可调用；重连后撤销能力立即生效。聚合跳过不支持成员并保留诊断；直接请求明确拒绝 |
| M02 | list/get 保留完整 frontmatter、文件 URI/digest/size、resultType=complete、ttlMs/cacheScope；分页不拆分单个 entry；直接 URI get 不依赖 listing | 实际协议验证分页、未列出但可 get 的 skill、未知 URI 的 -32602、错误/不完整 entry、保守 cache scope/TTL 与必需请求 _meta；普通 Resources 回归 |
| M03 | 聚合保留 host 分配的来源 label；只改 URI 前缀，不改名称/frontmatter/内容摘要；get/read 准确回到来源；不承诺未声明目录能力 | 两个上游同名/同 URI 不碰撞；支持文件和根/相对路径正确；不支持聚合的合法 URI scheme 可经直接路由访问；错误成员、空列表和目录拒绝不伪装成激活 |

上游旧实现缺少缓存提示时，若沿用现有保守归一化，须明确记录兼容策略与
实际输出；不得补造文件清单、大小、摘要或内容信任。聚合可以部分列举，不能
把空/部分列表解释成不存在其他 skills。网络错误与协议拒绝保留区别。

## 4. Native Host：A28

M04–M10 通过现有 Harness/资源准入/审批/持久化边界实现，不新建执行循环、
镜像任务库或独立选择服务。保留 local 与 MCP 两种来源的身份，不强制把远端
skill 重解释成本地目录，或伪造整个内容已下载。

| ID | 必须成立 | 必需本地证据 |
| --- | --- | --- |
| M04 | MCP 身份为 host 分配的 server label/配置身份与原 skill URI，带授权 scope；列举和显式 URI get 均可进入同一准入路径；不凭名称、URI scheme 或 serverInfo.name 判断身份。所有 scheme 都校验显式 SKILL.md 和父目录名与 frontmatter.name 一致，不解析 URI authority 为网络主机 | 同名本地/两个上游独立、URI 未列出仍能显式加载、未获许可/跨 workspace/未知 URI 拒绝；合法其他 scheme 与结构错误分别验证；任何内容读取前先完成元数据准入 |
| M05 | 冻结完整 entry；连接、listing、批准时不预取文件。实际加载时才读 SKILL.md，支持文件使用时才读；每次使用前核对原始字节 size/digest，完整 frontmatter 与 entry 逐字段相等；正文始终是任务数据，不提升为 system 指令 | 实际上游请求计数；列举/批准零文件读取；正文和延迟支持文件的 size/hash/frontmatter 不一致都不能进入模型或工具；实际模型 user/context 含完整 entry 与原字节，system 不含正文；普通 resources/read 不隐式激活 |
| M06 | 加载时保留的 entry 至少存续到其 SKILL.md 离开模型上下文；后续读取仅限该 manifest。相对路径以 skill 根解析、绑定原 server；目录视图不扩大 manifest | 非清单文件、越界、目录新增文件被拒绝；refresh 后旧批准失效。跨源读取仅在明确逐次批准且显示两来源后进行，否则拒绝；Turn 结束但指令仍在上下文时继续保留 entry |
| M07 | 加载遵守 host policy；host 代码执行须显式逐 skill 批准，涵盖正文诱导的任意本地命令；第一版忽略 MCP allowed-tools 等扩大 grant 的字段。嵌套 SKILL.md 作为支持材料不激活，单独激活须新的明确同意 | Ask/pending/answer 复用既有身份/fence；未批准时零本地执行；父 skill 批准不涵盖子 skill/frontmatter；任何批准都不能扩大 caller grant 或绕过原工具审批 |
| M08 | 持久批准绑定完整 URI/digest 集合；增删改任一文件均撤销旧批准。缓存按需填充，保留 MCP 来源并排除所有 filesystem-skill discovery；第一版每次命中重算原字节 size/hash 并比对 held entry | 模型工具改缓存后拒绝使用；改 manifest 后需重新批准；daemon 重启/上游断开后仍标注 MCP 来源且执行门槛不变；只读权限或保存的 digest 标签不能代替重验 |
| M09 | 第一版 host 拒绝 resources=dynamic，必需项明确失败/可选项记录排除；gateway 仍可原样转发。能处理协议基准 512 文件/16 MiB 的清单；内容按预算逐项读取，不整包截断 | dynamic、完整基准清单、超 host/上下文预算、二进制内容和不支持编码分别有可解释结果；元数据预算不足不触发正文预取 |
| M10 | 原生请求含已验证正文、来源和精确 held entry；Turn/子任务持久绑定不能被 refresh 静默替换。恢复复用原绑定和经验证缓存，原字节不可取得则阻塞；解释查询保持只读 | 实际 CLI→daemon→Cargo MCP 上游→native 模型请求 journey；选择、延迟读取、撤销批准、取消、重启恢复与 explain 对应同一身份；查历史零联网/零新执行 |

本地目录 manifest 的 1,024 文件/32 MiB 配额，与 MCP wire/host 预算分别处理。
超过当前 gateway 聚合限制的本地包不因此成为可发布的 MCP skill；不新增导出。
dynamic 拒绝与超预算排除是明确的第一版策略，不宣称已验证任意远端实现。

MCP 内容有一致的 digest 不代表可信。持久批准与权限状态复用既有 store；source
strings、磁盘 receipt 和上游声明不能充当 caller 授权凭据。持久恢复遇到缺失
来源/配置或已撤销权限时保留 blocked/failed 事实，不重新分配或扩大 grant。

规范允许刷新 entry 处理 stale metadata；本产品同时保持 Turn 内版本固定。
发现 stale 时阻止使用不匹配文件；刷新和重新批准只为显式后续安全边界准备新
选择，不能替换当前 Turn 的 held entry。若原 SKILL.md 仍在上下文，其原 entry
和约束继续保留，不能因刷新另一版本提前释放。

## 5. 执行与完成边界

显式 native 选择后的来源、批准、延迟读取、完整性和恢复要求继续验证。
不再运行安装/Git/自动相关性选择的旧验收；不以历史通过数代替本轮证据。
Gateway、本地目录、native host 与外部 ACP 载体各自保留角色边界。
真实 provider/vendor/远端 host、hosted CI 和其他 OS 资格分别报告。
