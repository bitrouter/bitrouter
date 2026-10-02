# 配置状态契约：原始第一批第 1 项

状态：已实现并完成本地验收。分支 `codex/config-state-contract`；基础包含 PR #916 的 router/preset 迁移。

本项完成原始清单的“整体配置状态契约”，不是重复交付 router 专用状态字段。
目标：修改保存配置后，任何 local/remote、CLI/Code 状态视图都不得把尚未生效的
配置说成 running；查询证据不足时必须保留 unknown。

## 已确认的问题

1. 本地 IPC/SIGHUP 和远程 reload 的拒绝规则不同。本地路径曾允许启动时固定字段
   进入 routing-table snapshot，但监听器、认证及其他启动组件仍使用旧值。
2. 状态只比较 `Config.routers`，不能表达 providers、timeouts、control、plugins 等变更。
3. 客户端先读自己的配置，再拼接另一个 socket 返回的 live router 状态，可能混合目标。
4. 保存配置缺失或损坏被折叠为无状态；旧 daemon 缺少字段不能被当作配置一致。
5. 修改已保存 control socket 地址，或把 YAML 改坏后，客户端可能失去仍在运行的进程。
6. 初始化流程的文件写入成功与运行时生效没有充分区分。

## 实现边界

- 沿用当前 daemon、reload coordinator、管理 action 和共享报告，不构建新的配置控制平面。
- 全部 reload 入口采用一致的启动字段分类，在改变任何参与者之前拒绝需要重启的候选。
- 运行事实由目标进程提供；remote 只使用远端证据，禁止读客户端配置补偿。
- 保存状态、运行状态和生效所需操作分开表达。配置相等、reload 成功、进程可达是不同事实。
- partial/unknown/running reload 不能被标成完整应用；保留已有参与者结果和 daemon incarnation。
- 报告只暴露安全分类和已知状态，不输出密钥、prompt/params 值或这些值的散列。
- policy-lock/access-policy 等独立来源需要自己的比较证据，不能由 YAML 没变推断它们一致。
- 发现模型、OAuth、环境变量等派生输入必须与保存文件区分，明确实际覆盖范围。
- 不涉及 HTTP checker、检查回执或 guardrails 制品拆分。

## 验收清单

| ID | 场景 | 要求 |
| --- | --- | --- |
| C01 | 启动后保存配置未改 | 在有充分目标端证据时显示一致 |
| C02 | 修改 provider/timeout 等可重载配置 | saved 已变化，running 保持旧状态，提示 reload |
| C03 | 修改 listen/control/plugins/router 等启动配置 | 所有 reload 入口一致拒绝，提示 restart，运行状态不被污染 |
| C04 | 配置损坏、删除、无法读取 | 明确 saved 无效或不可用，不能报告一致；运行进程仍可查询 |
| C05 | 成功 reload、失败但未变更、部分应用、取消/未知 | 区分结果；有部分应用时不能声明单一完整 running revision |
| C06 | policy-lock/access-policy 独立变更 | YAML 未变也不得声称整体一致 |
| C07 | 本地指定其他 config + 实际 socket；远端客户端有不同配置 | 查询始终归属同一目标；远程不补偿本地状态 |
| C08 | stopped / 旧 daemon | stopped 只有保存证据；缺字段为 unknown，不默认为 false 或空一致 |
| C09 | CLI JSON/human、Code/TUI inspector、HTTP control | status 共享 `StatusReport`，`/state` 复用 `ConfigurationState`，无 UI 独立推断 |
| C10 | onboarding/policy init 写入 | 明确保存完成，不能宣称运行已生效 |
| C11 | 秘密值变化及无关格式/注释变化 | 敏感变更能检测且不泄露；不把注释变化误判为运行参数变化 |
| C12 | 配置 control_socket 编辑与进程生命周期 | 仍能定位实际运行端点；陈旧记录不能指向无关进程 |
| C13 | 零配置进程启动后创建 YAML | 保存文件不自动成为运行来源；提示重启，reload 不能静默忽略新文件 |

## 实现与验证记录

### 已实现的契约

- `reload::ConfigurationState` 为可选的兼容字段，复用现有 reload coordinator 的
  instance/generation 和最后回执。CLI/Code 的 `StatusReport`、IPC status 和远程
  `/control/v1/state` 保留同一类型；旧服务缺字段不推断为一致。
- 文件来源在启动时只读取一次，保存解析前的语义文档作为私有比较基线；成功完成
  所有 reload 参与者后才推进基线。注释变化不算参数变化，敏感值只在进程内比较。
- `saved` 表达 `available/generated/missing/invalid/unavailable`；`running` 表达
  `in_sync/reload_required/restart_required/mixed/unknown`。独立 policy-lock 和
  access-policy 目录单独比较；没有足够证据时不报告整体一致。
- `mixed_state_history` 保留部分应用及中断的原因，后续准备阶段失败不会覆盖这些证据。
- 所有 reload 入口在应用前拒绝启动字段变更。旧代码中“本地成功替换配置快照，
  但真实监听器仍旧”的路径已取消。
- 每个逻辑配置文件有独立端点记录；记录不含配置内容，以绝对逻辑路径识别来源，
  只规范化父目录，不跟随配置文件本身的符号链接。进程退出时只删除属于自己的记录。
  读取时验证 PID 和进程实例，探测上限为 2 秒。现存配置优先级及显式 `--socket`
  保持权威；不会扫描其他配置的 daemon。
- `init` / `policy init` 的文件写入报告区分保存和生效，指导用户查询 status。

### 验证入口

| 验收 | 自动化证据 |
| --- | --- |
| C01–C04、C07、C11 | `tests/configuration_state.rs` 的真实 IPC：凭据与注释变更、成功重载、监听地址拒绝、错误来源隔离、无效/删除文件 |
| C03、C05 | `reload.rs` 的启动字段拒绝、准备超时、参与者故障、进行中和中断测试 |
| C06 | `auxiliary_source_drift_and_invalidity_prevent_in_sync`：只改独立文件、损坏及删除 policy-lock |
| C08–C09 | status、remote_control、human report 测试；Code/TUI 保持使用共享 report |
| C10 | onboarding 和 policy 输出测试：保存完成与运行生效分离 |
| C12 | 真实 `bro serve/status/stop` 子进程测试；locator 的实例校验、陈旧记录、权限、删除重建及超时测试 |
| C13 | `default_source_requires_restart_when_a_config_file_appears`：零配置运行期间新增有效/无效 YAML，以及来源切换的 reload 拒绝 |

最终本地验证（2026-09-15）：

- `cargo nextest run --workspace --all-features --no-fail-fast`：3,405 项通过，22 项跳过。
- `cargo clippy --workspace --all-features --tests -- -D warnings`：通过。
- `cargo fmt -- --check`、`git diff --check`：通过。
- `cargo test --workspace --all-features --doc`：5 项通过，1 项忽略。
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`：通过。
- `cargo run -p dist-helper -- check`：通过，schema 和 registry 产物一致。

以上为本地验证记录，不代表尚未触发的远程 CI 或生产环境验收。

### 保留的边界

- `in_sync` 覆盖被检查的配置输入，不证明外部 OAuth、模型发现结果、远程 registry
  或上游服务始终未变，也不替代连通性检查。
- 远程 `/state` 的配置证据与外层 coordinator 状态是先后采样；并发 reload 时，
  消费者必须核对 instance/generation，不能把不同代次拼成同一快照。
- 老版本 daemon 没有配置证据和实例标识时，配置状态保持未知，不能获得新的稳定端点定位保证。
- 本项不包含请求 checker、检查回执或独立 guardrails 制品；仍不代表原始第一批六项全部完成。
