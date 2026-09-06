---
type: fixed
title: "`bitrouter reload` rebuilds the policy table instead of silently keeping the old one"
pr: 816
---

`bitrouter reload` returned `{"status":"reloaded"}` while the daemon kept
serving the tiers it started with — only `restart` applied a `policy_table:`
edit. A success-reporting silent no-op.

If you have been restarting the daemon to pick up policy edits, `reload` is now
enough.
