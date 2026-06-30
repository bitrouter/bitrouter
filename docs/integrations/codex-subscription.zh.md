---
title: Codex subscription
description: 通过 Codex 后端把你的 ChatGPT 套餐接入 BitRouter——OAuth 登录，无需 OpenAI API 密钥。
sourceHash: ebdaa5b67dcae3c46e127d28467be57ed8f2624e8058c61fe9bc748695cde34d
---

有 ChatGPT Plus 或 Pro 套餐？通过 **Codex** 后端把它当作一个模型来源使用。`bitrouter providers login openai-codex` 会运行与 OpenAI 的 Codex CLI 相同的 OAuth 流程，保存一个可自动刷新的 token，并把它附加到发往 `openai-codex` 供应商的请求上——这样你的 ChatGPT 订阅就能覆盖这些 token，无需 `OPENAI_API_KEY`。

<Callout type="warn">
**`openai-codex` 不是 `openai`。** 这是一个独立的供应商。它访问的是 ChatGPT 的 Codex 后端（`chatgpt.com/backend-api/codex`），只支持 **Responses API**，且仅以你订阅的 OAuth 进行鉴权——它**不会**与标准的 `openai`（API 密钥）供应商共享端点或凭据。如果需要按 token 付费访问公开的 OpenAI API，请改用带密钥的 `openai` 供应商。
</Callout>

## 登录

```bash
bitrouter providers login openai-codex
```

这会在浏览器中打开 OpenAI 的授权页面（PKCE 流程，使用一个固定的回环端口），授权后会把凭据保存到 `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`。该 token 会自动刷新——只需登录一次。如需移除：

```bash
bitrouter providers logout openai-codex
```

<Callout type="info">
**已经登录过 Codex 了？** 登录菜单提供**「从该厂商 CLI 导入现有会话」**选项——BitRouter 会直接采用 Codex CLI 已保存的凭据（macOS 钥匙串或 `$CODEX_HOME/auth.json`），无需重新在浏览器中登录。导入的 token 与其他 token 一样会自动刷新。
</Callout>

### 多账号

凭据以 `(provider, label)` 为键。使用 `--label` 即可保留多个账号：

```bash
bitrouter providers login openai-codex --label work
```

## 路由到它

无需任何 `bitrouter.yaml` 配置块——`openai-codex` 是内置的，已保存的凭据会在请求时被自动发现。用注册表 id 指定你的套餐所暴露的模型：

```bash
bitrouter route openai-codex:gpt-5-codex
```

然后[启动 BitRouter 并发送请求](/docs/integrations/models#start-bitrouter-and-send-a-request)。使用带供应商前缀的 id `openai-codex:<model>` 可以把请求固定到你的订阅上；Codex 后端所提供的具体 id 见 [模型](/docs/concepts/models)。

## 延伸阅读

- [Codex](/docs/integrations/codex)——把 Codex *CLI* 指向 BitRouter（作为运行容器），这与此处「把套餐当模型来源使用」是两回事。
- [模型回退](/docs/features/model-fallback)——在过载时从你的订阅回退到另一个来源。
