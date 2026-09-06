---
type: removed
breaking: true
title: "`bitrouter launch --tui` hosted mode is removed"
pr: 816
---

The `--tui` flag is gone, along with `spawn::exec_hosted`, the `hosted_env`
overlay, and the PTY host, VT adapter and terminal emulator behind them.

Plain `bitrouter launch` — the inherited-terminal path — is unchanged and
remains the recommended way to run a harness. If you passed `--tui`, drop the
flag. For an in-terminal session over ACP, `bitrouter chat <agent>` is the
replacement surface.

The effort moved to the ACP agent endpoint instead, so every ACP client
benefits rather than only the one terminal we shipped. Four dependencies went
with it (`alacritty_terminal`, `portable-pty`, `termwiz`,
`wezterm-input-types`).
