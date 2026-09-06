---
type: removed
breaking: true
title: "The curated registry is scoped to ten core labs"
pr: 823
---

The catalog now carries models from the ten labs BitRouter routes to — OpenAI,
Anthropic, Google (Gemini), xAI, DeepSeek, Alibaba (Qwen), MoonshotAI, Z.ai,
MiniMax and Xiaomi (MiMo). Five model ids outside that set are removed, along
with every provider offering of them across 14 provider files:

| Removed model | Why |
| --- | --- |
| `google/gemma-4-31b` | Gemma, not Gemini |
| `stepfun/step-3.5-flash` | StepFun |
| `stepfun/step-3.7-flash` | StepFun |
| `meituan/longcat-2.0` | Meituan |
| `tencent/hy3` | Tencent (Hunyuan) |

Four providers are deleted with them — `stepfun`, `stepfun_cn`,
`stepfun_step_plan`, `stepfun_step_plan_cn` — each served only StepFun models
and would otherwise carry an empty catalog. `tencent` and `tencent_cn` stay:
TokenHub re-serves DeepSeek, GLM, Kimi and MiniMax there.

A config naming one of those model ids, or one of the deleted providers, no
longer resolves. Point it at a model from a supported lab, or declare the model
on your own provider entry.
