---
type: changed
breaking: true
title: "Setup, status and review JSON report `bitrouter/auto` instead of `@auto`"
pr: 788
---

The `model` field in the optimize setup, status and review JSON reports changes
from `@auto` to `bitrouter/auto`. A script matching the literal `@auto` needs
updating to the slash form.

The optimizer's harness environment still composes `@{preset}`: it is generic
over preset name, and the reserved namespace only claims `auto`, so composing
the slug there would `400` every non-`auto` optimization lineage.
