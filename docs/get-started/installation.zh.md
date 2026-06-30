---
title: 安装
description: 安装开源的 BitRouter 二进制并在本地运行，或登录 BitRouter Cloud——两种方式核心完全一致。
sourceHash: 3e9c4c0d2e7d148bb2cdf6f0abf5e02101e9a3da5de30dd3f091335a5c59423e
---

BitRouter 有两扇入口，且都运行**同一套开源核心**。用你自己的密钥自托管二进制，或登录 BitRouter Cloud——路由引擎完全相同。云端账号在其之上额外提供了什么，见 [自托管 vs 云端](/docs/get-started/self-hosted-vs-cloud)。

## 自托管二进制

安装开源二进制（Apache 2.0）：

<Tabs items={['macOS / Linux', 'Homebrew', 'npm', 'cargo']}>
<Tab value="macOS / Linux">

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh
```

</Tab>
<Tab value="Homebrew">

```bash
brew install bitrouter/tap/bitrouter
```

</Tab>
<Tab value="npm">

```bash
npm install -g bitrouter
```

</Tab>
<Tab value="cargo">

```bash
cargo install bitrouter
```

</Tab>
</Tabs>

把供应商密钥设到环境里，然后启动代理：

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GOOGLE_API_KEY also work
bitrouter start
# Proxy running at http://127.0.0.1:4356
```

BitRouter 在启动时自动检测环境中的任意密钥——无需配置文件。任何已设置密钥的供应商立即可用。完整的识别变量列表见 [BYOK](/docs/features/byok)，或参阅 [本地与私有模型](/docs/integrations/models) 把 BitRouter 指向 Ollama、vLLM 或 LM Studio，完全免费。

如需高级路由规则、护栏或多账号故障转移，可生成一份配置文件：

```bash
bitrouter init          # writes ./bitrouter.yaml (override with `-c <path>`)
bitrouter start
```

## 使用 BitRouter Cloud

从终端登录 BitRouter Cloud 账号——一个账号即可覆盖托管网络提供的所有模型，无需任何上游供应商密钥：

```bash
bitrouter auth login    # RFC 8628 device flow against api.bitrouter.ai
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

你也可以不运行本地二进制，直接把 agent 指向托管端点。无论哪种方式，核心都是同一套——云端账号是账号与网络，而非另一套部署。完整说明见 [自托管 vs 云端](/docs/get-started/self-hosted-vs-cloud)，模型目录与价格见 [托管供应商](/docs/get-started/models-and-providers)。

## 把你的 agent 指向代理

无论你以何种方式启动，BitRouter 都是一个即插即用的代理。把你的 agent 运行时指向代理的基础 URL——自托管时为 `http://127.0.0.1:4356`——每一次模型调用都会经由 BitRouter 路由，且无需修改任何 harness。

<Cards>
  <Card title="快速开始" href="/docs/get-started/quickstart" description="一分钟内让 agent 经由 BitRouter 路由" />
  <Card title="自托管 vs 云端" href="/docs/get-started/self-hosted-vs-cloud" description="核心完全一致；云端账号额外提供了什么" />
</Cards>
