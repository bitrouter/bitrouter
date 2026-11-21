# CLI、目录 skills 与本地编排

`bitrouter` 管理网关、模型路由、租户资源和 Eval。 `bro` 持有本地 workspace、
工具批准、Thread checkpoint 和 ACP 进程。两者通过 gateway 的 HTTP/SSE 接口连接。
可用命令见 [CLI reference](../CLI.md)。

## 本地 skills

用 `bro skills list` 查找 workspace 下的 skills，通过 `--global` 或 `--root PATH`
指定其他范围。 `show`、`read`、`write` 和 `init` 直接操作选定的目录。
分发的 agent skill 由 `npx skills add bitrouter/bitrouter` 安装。

用 `bro thread create --skill NAME` 或 `bro task run PROMPT --skill NAME` 显式绑定
skill。BRO 保存本次绑定的指令和摘要，操作者可用
`bro skills explain --thread ID --turn ID` 检查该 Turn 使用的版本。目录选择和
工具授权见 [Harness resources](harness-resources.md)。

## Model Policy 与访问控制

`policy init`、`check`、`show`、`status`、`diff`、`publish` 和 `rollback` 使用同一份
[Policy source](../specs/policy/README.md)。`show` 默认为所选网关的 active 状态，
`--disk` 显式读取保存的源文件。返回的 `source` 可以重新提交，`source_digest` 用于
显式 CAS。网关鉴权使用 `access-policy` 命令。
