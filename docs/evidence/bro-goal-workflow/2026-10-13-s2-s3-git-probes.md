# S2/S3 Git primitive probes

日期：2026-10-13。仅为本机真实 Git 原语的有限证据；不是 Rust 测试或完整 BRO 执行验收。

两个探测均在 Ruby `Dir.mktmpdir` 的独立临时仓库执行，通过 `Open3.capture2e` 传递 argv，检查每个 Git 命令的 exit status；没有修改开发 checkout、推送或调用 provider。Ruby 仅是此次临时命令编排，不是产品或测试 fixture 依赖。

## 共同上游与冲突

1. 初始化临时仓库，提交 `tracked.txt=before`；创建 left、right、consumer、conflict 四个 detached worktree。
2. left/right 都写入相同 tracked 变更和 `common.txt`，各自新增 left/right 文件并 `git add -A`。
3. 对各分支执行 `git diff --binary <base> --`，将实际 stdout 保存为 patch。
4. consumer 依次执行 `git apply --3way --index --binary <left.patch>` 和相同形式的 right patch。两次 exit 0，common/left/right 三个文件存在。
5. conflict 写入不兼容 tracked 变更并产生 patch；consumer 应用它返回非零。

实际命令输出：

```text
Git flag probe: common dependency merge succeeded; incompatible change rejected.
```

该 probe 只验证选定的合并命令及返回码。`goal_artifacts` 的 Rust 文件边界、摘要校验、原子流程及调用关系仍需执行其测试。

## 基线时间戳

在另一临时仓库使用 `GIT_AUTHOR_DATE=@0 +0000`、`GIT_COMMITTER_DATE=@0 +0000` 创建空 commit，再执行 `git show -s --format=%at %ct`，实际输出为 `0 0`。

```text
Git date probe: epoch-zero baseline author/committer timestamps are exact.
```

这只确认时间戳格式；没有证明 BRO `prepare_workspace` 的整体行为、隔离或输入冻结。
