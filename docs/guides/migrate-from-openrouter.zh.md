---
title: 从 OpenRouter 迁移
description: 从 OpenRouter 迁移到 BitRouter 的完整指南
sourceHash: f8824812608f57f3f8a2d74827c279aeae1739971c7822f1722d2555fa60d3cb
---

# 从 OpenRouter 迁移到 BitRouter

本指南将带您完成从现有 OpenRouter 集成迁移到 BitRouter 的过程，重点介绍关键差异和优势。

## 为什么要迁移？

对于智能代理工作负载，BitRouter 相比 OpenRouter 提供了多项优势：

| 功能 | OpenRouter | BitRouter |
|------|------------|-----------|
| **部署方式** | 仅云端（闭源） | 自托管或云端（Apache 2.0） |
| **平台手续费** | 通过 Stripe 收取 5.5% | 稳定币 2% · Stripe 5% |
| **代理功能** | 基础路由 | 无界面 CLI、代理自主支付、MCP/ACP 网关 |
| **访问** | 需要注册账户 | 无需注册即可使用 |

## 迁移路径

### 步骤 1：获取 BitRouter API 密钥

**方式 A：无界面 CLI（推荐）**

安装 CLI 并登录——`bitrouter auth login` 通过 RFC 8628 设备码流程在浏览器中打开授权页，授权后会将 OAuth 凭证（自动续期）保存到 `$XDG_DATA_HOME/bitrouter/account-credentials.json`。此后通过 `bitrouter` provider 的每次请求都会自动携带凭证——无需在配置中写入 API 密钥：

```bash
npm install -g bitrouter
bitrouter auth login
```

使用 `bitrouter auth whoami` 查看本机会话；使用 `bitrouter cloud --help` 管理账户级资源（API 密钥、用量、计费、策略、BYOK、OAuth 客户端）。

**方式 B：控制台**

在 [cloud.bitrouter.ai](https://cloud.bitrouter.ai) 注册并从控制台复制您的 API 密钥。

### 步骤 2：更新 Base URL 和 API 密钥

迁移就这两步。将 OpenRouter 的端点和密钥替换为 BitRouter 的：

<Tabs items={['Python', 'JavaScript', 'curl']}>
<Tab value="Python">
```python
# 之前（OpenRouter）
client = openai.OpenAI(
    base_url="https://openrouter.ai/api/v1",
    api_key=OPENROUTER_API_KEY,
)

# 之后（BitRouter）
client = openai.OpenAI(
    base_url="https://api.bitrouter.ai/v1",
    api_key=BITROUTER_API_KEY,
)
```
</Tab>
<Tab value="JavaScript">
```javascript
// 之前（OpenRouter）
const client = new OpenAI({
  baseURL: 'https://openrouter.ai/api/v1',
  apiKey: OPENROUTER_API_KEY,
});

// 之后（BitRouter）
const client = new OpenAI({
  baseURL: 'https://api.bitrouter.ai/v1',
  apiKey: BITROUTER_API_KEY,
});
```
</Tab>
<Tab value="curl">
```bash
# 之前（OpenRouter）
curl https://openrouter.ai/api/v1/chat/completions \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "你好"}]}'

# 之后（BitRouter）
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "你好"}]}'
```
</Tab>
</Tabs>


## 可移除的请求头

如果您的 OpenRouter 集成中设置了以下请求头，可以安全删除：

| OpenRouter 请求头 | BitRouter 中的状态 |
|-------------------|--------------------|
| `HTTP-Referer` | 不使用 |
| `X-Title` | 不使用 |
| `transforms` | 不使用——守护栏在服务端配置 |
| `route` | 不使用——提供商路由在控制台中配置 |

## 下一步

<Cards>
  <Card title="快速开始" href="/docs/get-started/quickstart" description="一分钟内运行 BitRouter" />
  <Card title="配置" href="/docs/features/presets" description="高级路由和配置选项" />
  <Card title="集成" href="/docs/integrations" description="特定代理的集成指南" />
  <Card title="API 参考" href="/docs/reference" description="完整的 API 文档" />
</Cards>

## 需要帮助？

- **Discord**：[加入我们的社区](https://discord.gg/G3zVrZDa5C) 获取迁移支持
- **GitHub**：[报告问题](https://github.com/bitrouter/bitrouter/issues) 或贡献代码
- **Email**：contact@bitrouter.ai 企业迁移协助
