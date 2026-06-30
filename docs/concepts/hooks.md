---
title: Hooks
description: The pipeline model behind BitRouter — a request flows through ordered stages, and each hook can allow, deny, mutate, or observe it.
sourceHash: e6aea5071977bc2d85bd967dc77e4f1102cfd84a1236b1fa4033fc86b3966397
---

A **hook** is a callback that runs at a specific point as a request flows through the router. BitRouter processes every request as an ordered **pipeline of stages**; hooks attach to those stages, and depending on the stage a hook can **allow, deny, mutate, or observe** the request. [Plugins](/docs/concepts/plugins) bundle hooks and install them.

## The language-model pipeline

The main `language_model` pipeline runs five kinds of hook, in order:

1. **PreRequest** — auth, policy, rate-limit, balance, and guardrail checks. Each returns *allow* or *deny*; the first deny stops the pipeline and maps to an HTTP status.
2. **Route** — resolve or rewrite the ordered chain of routing targets for the request.
3. **Execution** — observe each attempt and control fallback on success or failure.
4. **Stream** — intercept streamed response parts to rewrite, drop, or abort them.
5. **Observe** — read-only observation at every stage boundary; it never influences the request, and a failure here can't break it.

Settlement — metering, charging, and receipts — runs against an immutable context once the request is done.

For the trait signatures and a worked example, see [Build a plugin](/docs/guides/build-a-plugin).
