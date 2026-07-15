---
title: 从 CLI 调用 BitRouter Cloud API
description: 使用类似 gh api 的命令，通过 OAuth 或 API-key 登录调用 models、Chat Completions、Messages、Responses 与 Google Generative AI。
---

`bitrouter cloud api` 会直接向 BitRouter Cloud 发送已认证请求。它复用 `bitrouter cloud login` 的凭证，因此脚本无需在不同配置文件之间搬运 secret，也无需启动本地 daemon。

## 登录

开发工作站可使用浏览器 OAuth；CI 中可传入已有的 `brk_` key：

```bash
bitrouter cloud login
bitrouter cloud login --api-key "$BITROUTER_API_KEY"
bitrouter cloud whoami
```

两种方式都会写入 `$XDG_DATA_HOME/bitrouter/account-credentials.json`，Unix 上权限为 `0600`。`whoami` 会报告 `authentication: oauth` 或 `authentication: api_key`，但不会打印 bearer。API-key 登录不发起网络请求。

<Callout type="warning">
请像上面的示例一样通过环境变量传递 API key。直接写在命令行中的 key 可能被 shell history 保留，也可能被进程检查工具看到。
</Callout>

<Callout type="warning">
只传入 `/v1/models` 这样的相对端点。绝对 URL、scheme-relative 路径与 fragment 会被拒绝；redirect 跟随则被禁用，确保已存储凭证不会离开登录 origin。
</Callout>

## 列出模型

未提供字段或输入 body 时，默认方法为 `GET`：

```bash
bitrouter cloud api /v1/models
```

显式使用 `GET` 时可通过字段添加 query 参数：

```bash
bitrouter cloud api /v1/models -X GET -F owned=true -F limit=25
```

## Chat Completions

保存一个 OpenAI-compatible 请求：

```json
{
  "model": "openai/gpt-5",
  "messages": [
    { "role": "user", "content": "Explain speculative decoding." }
  ],
  "stream": true
}
```

然后发送其精确字节：

```bash
bitrouter cloud api /v1/chat/completions --input chat.json
```

## Messages

Anthropic-compatible Messages 路由使用同一份已存储凭证：

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "max_tokens": 512,
  "messages": [
    { "role": "user", "content": "Explain speculative decoding." }
  ]
}
```

```bash
bitrouter cloud api /v1/messages --input messages.json
```

## Responses

Responses 请求既可来自文件，也可由字段构造：

```json
{
  "model": "openai/gpt-5",
  "input": "Explain speculative decoding.",
  "stream": true
}
```

```bash
bitrouter cloud api /v1/responses --input responses.json
bitrouter cloud api /v1/responses \
  -f model=openai/gpt-5 \
  -f input="Explain speculative decoding." \
  -F stream=true
```

## Generate Content

Google Generative AI 将 model action 放在路径中：

```json
{
  "contents": [
    {
      "role": "user",
      "parts": [{ "text": "Explain speculative decoding." }]
    }
  ]
}
```

```bash
bitrouter cloud api \
  /v1beta/models/google/gemini-2.5-flash:generateContent \
  --input generate-content.json
```

## Stream Generate Content

使用 streaming action 获取 SSE 输出：

```bash
bitrouter cloud api \
  /v1beta/models/google/gemini-2.5-flash:streamGenerateContent \
  --input generate-content.json
```

stdout 被重定向时，response chunk 会在到达时立即写出，并保留每一个字节，因此普通 pipeline 可以安全使用：

```bash
bitrouter cloud api /v1/chat/completions --input chat.json > events.sse
```

## 使用字段构造请求

`-f/--raw-field` 始终创建字符串。`-F/--field` 会把 `true`、`false`、`null` 与整数转换为 JSON 值，并将 `@file` 或 `@-` 作为字符串读取。Bracket 语法可构造嵌套对象与数组；不带 `=` 的 `key[]` 会创建空数组：

```bash
bitrouter cloud api /v1/chat/completions \
  -f model=openai/gpt-5 \
  -f 'messages[][role]=user' \
  -f 'messages[][content]=Hello' \
  -F stream=true \
  -F max_tokens=256
```

字段会让隐式方法变为 `POST`，并组成 JSON body。显式使用 `GET` 时，或 `--input` 已占用 body 时，字段会变成 query 参数。只有一个字段或 `--input -` 可以消费 stdin。

## 控制 header 与输出

```bash
bitrouter cloud api /v1/models -H 'Accept: application/json'
bitrouter cloud api /v1/models --include
bitrouter cloud api /v1/models --silent
bitrouter cloud api /v1/models --verbose
```

`-H/--header` 可重复。提供 `Authorization` 会覆盖该次请求的已存储 bearer。`--include` 在 body 前写入 status line 与 header。`--silent` 会 drain body，但不打印。`--verbose` 将脱敏后的 request/response metadata 写入 stderr，并与 `--silent` 冲突。

交互式 JSON 会被 pretty-print。非 TTY JSON、任意 binary body 与 SSE 都不会被重新格式化。对于 HTTP 4xx/5xx，response body 保留在 stdout，诊断写入 stderr，命令以非零状态退出。

## 首批范围

首个版本有意省略 GitHub 专属的 `gh api` 功能：GraphQL、pagination/slurp、`--jq`、Go template、cache、hostname 选择、preview header 与 placeholder expansion。它支持任意相对 BitRouter Cloud 端点；首批有文档与测试覆盖的矩阵为：

- `GET /v1/models`
- `POST /v1/chat/completions`
- `POST /v1/messages`
- `POST /v1/responses`
- `POST /v1beta/models/{model}:generateContent`
- `POST /v1beta/models/{model}:streamGenerateContent`

完整旗标参考见 [CLI 参考](/docs/concepts/cli)。
