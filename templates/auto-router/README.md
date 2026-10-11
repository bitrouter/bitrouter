# Unified `bitrouter/auto` routing

Native `bro code` already defaults to `bitrouter/auto` using the policy bundled
with the binary. That default shares this template's tiers and empty routes,
without the progress guard that requires trajectory configuration. No policy
file or model/output-token flag is needed. This directory demonstrates an
explicit file-backed policy with trajectory protection enabled.

Start this directory with `bro serve --config bitrouter.yaml` and request
`bitrouter/auto`. The named router binds one version 4 policy for model, optional
reasoning effort, and context treatment. API requests and Core sessions share
the same System One classification and planning stages. Rich request history can
supply workflow state; Core adds explicit task and immutable evidence facts.

This bootstrap uses the strong default and contains no pretrained routes.
Old deterministic scorecard experiments are not evidence for a different
classifier. Add evaluated routes through the existing Eval/Optimize workflow;
each explicit semantic route requires a matching predictor contract and route
certificate. Frozen mode records evidence without publishing policy changes.

Every action is structured:

```yaml
tiers:
  strong: { model: "openai-codex:gpt-5.6-sol", effort: high, context: preserve }
  economy: { model: "bitrouter:deepseek/deepseek-v4-pro", context: evidence }
```

`preserve` retains full context. `evidence` allows the shared planner to consider
an owner's admitted extracts, summaries, omissions, and recalls. It grants no
new access. Ordinary HTTP prompts retain their full context unless a trusted
host supplies alternatives and the necessary capabilities. Missing effort
preserves the caller's effort.

To enable semantic classification, add `decision_model` with a model available
on your decision backend and set the credential environment variable:

```yaml
decision_model:
  model: your-decision-model
  base_url: https://api.typesafe.ai
  api_key_env: TYPESAFE_API_KEY
```

The built-in HTTP adapter speaks TypeSafe's typed decision protocol. Other
backends, including Jev, can implement SDK `DecisionExecutor`; using a different
URL alone is sufficient only when its protocol is compatible. No backend name
changes the routing algorithm. With no backend or an invalid/low-confidence
answer, the relevant labels abstain and the policy default remains available.

Classification receipts record backend, rubric, threshold, input commitment and
reported distributions. Plan receipts bind the exact selected prompt. Learn
uses those receipts alongside action propensities and independently attributed
request, episode or task outcomes. A successful request is not task success.

Version 1–3 locks, scalar action targets, the global `policy_table` entry, and
`decision_model.policy.generation_models` are rejected. Write a fresh version 4
lock; there is no automatic migration of old quality evidence.
