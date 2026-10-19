# macOS loader diagnosis

2026-10-13。用户明确澄清此前没有执行重启/服务恢复，并要求排查。此记录不改变 BRO 产品语义、系统授权或安全设置。实现 Goal 保留原范围，编译 gate 仍未通过。

## 当前证据

| 检查 | 实际结果 | 能支持的结论 |
| --- | --- | --- |
| `sw_vers` | macOS 26.2 / 25C56 | 本次执行主机版本 |
| `rustc --version` | rustc 1.97.0，正常退出 | 已安装 compiler 主程序可以启动；不能据此推断 proc-macro 可加载 |
| `xcode-select -p` | `/Library/Developer/CommandLineTools` | CLT 被选中 |
| `codesign --verify --strict --verbose=4`，当前 serde_core build script | exit 0，valid on disk / satisfies Designated Requirement | 未发现本文件签名损坏；不等于 runtime policy 允许 |
| `xattr -l`，同文件 | 仅观察到 com.apple.provenance；无 com.apple.quarantine | 没有 quarantine 删除修复的依据 |
| `spctl --status` | assessments enabled | Gatekeeper 仍启用 |
| `spctl --assess --type execute --verbose=4`，同文件 | exit 3 / rejected；签名为 linker-signed adhoc | standalone 评估被拒绝；本地 adhoc 产物的这个结果本身不能证明启动停顿的根因 |
| 系统 daemon | `com.apple.security.syspolicy`，PID 490，连续约 15 天，约 100% CPU；launchctl 显示 runs 1、never exited | 用户澄清与 live state 一致；服务未重启。CPU 高不是根因证明 |
| 本次五分钟 log 查询 | 重复 SecTrustEvaluateIfNecessary；一段记录反复出现 `Unable to initialize qtn_proc: 3` 和 `dispatch_mig_server returned 268435459` | 执行评估路径有错误；尚未将错误绑定到特定请求或确定其根因 |
| 后续 30 秒窄 log 查询 | 未返回匹配 Codex/EMFILE/100024 或 kernel denial 的行 | 不能断言有 file-descriptor 泄漏；短窗口没有匹配也不能排除 |
| 非 root `lsof`、只读 TCC DB、`sudo -n true` | lsof 无可用结果；TCC authorization denied；sudo 需要密码 | 当前自动执行进程没有取得管理员只读诊断权限；不能把不可见 fd 数报告为 0 |

初次 `launchctl print system/com.apple.syspolicyd` 的服务名错误已通过读取 LaunchDaemons 纠正为 `com.apple.security.syspolicy`；不是服务不存在。

## 独立于 Cargo / Rust / BRO 的复现

在本任务私有目录 `/tmp/bro-loader-probe-pYnYNS`（0700）创建如下 C 程序，使用系统 clang 编译：

```c
#include <stdio.h>
int main(void) {
    puts("bro loader probe reached main");
    return 0;
}
```

- `clang .../probe.c -o .../probe`：exit 0。
- 该 binary 的 `codesign --verify --strict --verbose=4`：exit 0；无 quarantine，仅 provenance。
- 从本任务 exec 启动：session 4409 / PID 44833，持续约五分钟无输出；[live sample](2026-10-13-independent-c-loader-sample.txt) 显示 `_dyld_start + 0`、96K、no binary images，没有进入 main。
- 将系统 `/usr/bin/true` 字节复制到同一私有目录，copy 的 signature verify 也通过；copy 的启动 session 48086 / PID 44919 持续约三分多钟无退出。这个结果不证明受保护原安装路径的 Apple 工具也不能启动。
- 两个本任务启动的 probe 随后 Ctrl-C，工具均返回 exit 130；最终精确 PIDs 检查均已不存在。原 `/usr/bin/true` 没有修改。保留 probe source/binary 供用户手动对照，不把临时 probe 加入产品或 test fixtures。

结论：新增 Cargo 源码不是此次独立 C probe 启动停顿的必要条件；故障至少延伸到该执行上下文中的新可执行文件加载/评估路径。尚未证明是所有启动应用、所有用户上下文或系统全局故障。

## 启动应用与权限

实际 exec 父进程是 `/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex`，来自 ChatGPT.app 26.1007.30912；不是假定的 `/Applications/Codex.app`。

只读查看 System Settings → Privacy & Security → Developer Tools：计数 0；列表仅 Terminal 且开关 off，ChatGPT 不在列表。未点击 Add、开关或 authentication dialog，未改 TCC、Gatekeeper、policy DB 或服务。

Apple [Developer Tools 文档](https://support.apple.com/guide/mac-help/control-ability-apps-run-software-meet-mchlc5fb7f9c/mac) 说明这项权限控制哪些应用可以运行不满足系统安全策略的本地软件。这是一个待区分的授权假设，不能仅凭列表断言它导致 daemon 停顿。

自动化工具明确拒绝 Terminal：`Computer Use is not allowed to use the app 'com.apple.Terminal' for safety reasons.` 没有通过 AppleScript、shell UI 控制或另一个入口绕过限制。用户在自己的 Terminal 执行同一 `/tmp/.../probe`，回复“同样卡住，没有输出”。只读进程检查发现 PID 46312、父进程 zsh 45831；[Terminal probe sample](2026-10-13-terminal-c-loader-sample.txt) 同样显示 `_dyld_start + 0`、96K footprint、没有 binary images。本任务没有终止用户的这个进程。

这个对照证明问题不限于 ChatGPT 的启动上下文；不能据此推断所有应用启动都受影响，也不能直接确定 Developer Tools 权限或 syspolicyd 是根因。此时 syspolicyd PID 490 仍约 98% CPU，launchctl 仍显示 runs 1、never exited；新的一分钟窄日志查询没有返回 qtn_proc、dispatch_mig_server 或 fd 耗尽的匹配记录。

## 外部资料与未证实假设

- Apple [Resolving Gatekeeper Problems](https://developer.apple.com/forums/thread/706379) 建议先验证签名，并区分 signature、notarization 与 CLI 入口问题。
- OpenAI repository 的 [issue 26117](https://github.com/openai/codex/issues/26117) 有不同机器/版本上 syspolicyd fd 耗尽与 CLI 启动停顿的第一手报告，归为 [issue 25882](https://github.com/openai/codex/issues/25882) 的重复；原始 [25243](https://github.com/openai/codex/issues/25243) 阅读时仍 open。它们不是当前设备的 fd、触发者或修复证据，不能直接套用旧版 Codex.app 的因果结论。

## 管理员只读诊断与恢复尝试

用户在自己的 Terminal 结束探针并运行管理员诊断。第一次仅重定向 stdout 的两个文件为 0 bytes，没有诊断数据；修正为显式 `sample -file` 与 stdout/stderr 捕获后取得报告。密码只在用户本机输入，本任务没有取得密码或管理员 shell。

- [服务 sample 摘录](2026-10-13-syspolicyd-sample-excerpt.txt)：一秒采样的 802 次观察中，`syspolicyd.evaluations.scans` 串行队列始终处于 `SecStaticCodeCheckValidityWithErrors → MachORep::signingData()`；638 次顶层 `_sigtramp`，164 次 `Security::Universal::architecture()`。与反复观察到的约 98% CPU 一致，强烈提示签名扫描异常循环；尚未确定被扫描文件、具体信号或初始触发原因。
- [管理员 lsof](2026-10-13-syspolicyd-lsof.txt)：35 个对象行，其中 numeric fd 为 0–17，共 18 个，没有支持当前 fd 耗尽的证据。不能把旧版 Codex issue 的 fd 泄漏原因套到本机。
- Apple [公开 MachORep 源码](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_codesigning/lib/machorep.cpp) 说明 `signingData()` 获取架构并读取内嵌签名；用于解释调用栈，不证明公开 main 与本机 Security 61901.60.40 二进制完全一致，也不证明哪个客户端引发异常。
- 本机 SDK `mach/message.h` 将此前日志的 decimal `268435459` 定义为 `MACH_SEND_INVALID_DEST`（0x10000003），不是 fd 耗尽错误；该日志与本次采样没有请求级关联。
- 用户随后按建议执行 `sudo /bin/launchctl kickstart -k -p system/com.apple.security.syspolicy`。实际输出：`Could not kickstart service "com.apple.security.syspolicy": 150: Operation not permitted while System Integrity Protection is engaged`。再次读取 launchctl：runs 1、PID 490、never exited；服务重启没有成功，不报告已恢复。
- 服务尝试之后本任务的同一 probe session 93276 仍无输出，随后 Ctrl-C 返回 exit 130。没有遗留本任务诊断 probe，没有关闭 SIP/Gatekeeper、授权 Developer Tools、删除 policy DB 或修改程序签名来绕过问题。

下一步建议用户保存其他应用的工作后，通过 Apple 菜单整机重新启动；本任务不自动重启主机。回来先验证同一 C probe 是否输出 `bro loader probe reached main`，再重试一次针对性 Rust 测试/编译。只有实际运行成功才解除构建 blocker；整机重启也只是恢复与因果对照步骤，不是永久修复或根因证明。

## 整机重启后的实际对照

后续 Goal 回合发现 PID 490 已不存在。当前 `kern.boottime` 为 2026-10-13 12:58:00 -0400，uptime 约 14 分钟，syspolicyd PID 491、CPU 0.0%，launchctl state running。这是实际整机重启证据，不依赖用户选项或意图推断。

原 `/tmp/bro-loader-probe-pYnYNS` 已被清理。在新的 private 目录 `/tmp/bro-loader-recheck.lb0362` 用同一 C 源码和系统 clang 重新编译、启动，命令 exit 0，实际输出 `bro loader probe reached main`。随后 `cargo test -p bitrouter-tui --lib goals:: -- --nocapture` session 18100 正常推进 serde/darling 等依赖编译，未复现原 loader 停顿；最终测试结果另记到 implementation ledger，尚不能仅凭开始编译报告所有 Rust gate 恢复或通过。

恢复对照强化了签名评估服务异常与启动故障关联，但没有确定最初触发的程序、文件或永久修复；没有据此归因 BRO、Rust 或某一 OpenAI 应用。

后续 continuation 再次实际运行 `/tmp/bro-loader-recheck.lb0362/probe`，仍输出 `bro loader probe reached main` 并 exit 0；syspolicyd PID 491、CPU 0.0%。当前 Rust 编译、45 项 Goal/API/PTY 回归、workspace Clippy 与独立 doctest 已完成，结果和适用源码见 implementation ledger。原 `/tmp/bro-syspolicyd-restart.txt` 已不存在；此前保存的 kickstart 失败记录仍保留，不将当前恢复写成该命令成功的结果。当前没有 loader 构建 blocker，最初触发原因仍未确定。

本轮又实际运行同一 probe，仍正常输出并 exit 0；当前 Rust 已完成 72 项相关回归、workspace/all-target/all-feature Clippy 和独立 doctest。终端缩放丢键是独立问题，区分实验和修复见 [terminal input diagnosis](2026-10-13-macos-terminal-input-diagnosis.md)。本次未修改系统安全设置或再次重启服务；启动恢复不能归因于此前被 SIP 拒绝的 kickstart。MVP 的真实 provider/harness 与完整验收仍未完成。
