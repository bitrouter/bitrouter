---
title: 结构化输出
description: 跨所有提供商强制执行 JSON 模式 — 自动为 OpenAI、Anthropic、Google 等进行协议转换。
sourceHash: 6a4771c946e6a64814738de8566a6afa1971681ee5d7ad0c72b25ed12359ec59
---

**结构化输出**确保模型响应符合您指定的 JSON 模式。BitRouter 提供统一的 API，无缝支持所有主要提供商 — 自动在各个提供商的原生格式之间进行转换。

## 快速开始

定义一次模式，随处使用。BitRouter 处理协议转换：

<Tabs items={['OpenAI SDK', 'Anthropic SDK', 'cURL']}>
<Tab value="OpenAI SDK">

```typescript
import OpenAI from 'openai';

const client = new OpenAI({
  baseURL: 'https://api.bitrouter.ai/v1',
  apiKey: process.env.BITROUTER_API_KEY,
});

const completion = await client.chat.completions.create({
  model: 'claude-3-5-sonnet-latest',
  messages: [
    { role: 'user', content: '从这篇文章中提取关键事实...' }
  ],
  response_format: {
    type: 'json_schema',
    json_schema: {
      name: 'article_facts',
      strict: true,
      schema: {
        type: 'object',
        properties: {
          title: { type: 'string' },
          author: { type: 'string' },
          key_points: {
            type: 'array',
            items: { type: 'string' }
          },
          sentiment: {
            type: 'string',
            enum: ['positive', 'neutral', 'negative']
          }
        },
        required: ['title', 'key_points', 'sentiment']
      }
    }
  }
});

// 响应保证匹配您的模式
const facts = JSON.parse(completion.choices[0].message.content);
```

</Tab>
<Tab value="Anthropic SDK">

```python
import anthropic

client = anthropic.Anthropic(
    base_url="https://api.bitrouter.ai",
    api_key=os.environ["BITROUTER_API_KEY"],
)

message = client.messages.create(
    model="gpt-4o-2024-11-20",
    messages=[
        {"role": "user", "content": "从这篇文章中提取关键事实..."}
    ],
    output_config={
        "format": {
            "type": "json_schema",
            "schema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "author": {"type": "string"},
                    "key_points": {
                        "type": "array",
                        "items": {"type": "string"}
                    },
                    "sentiment": {
                        "type": "string",
                        "enum": ["positive", "neutral", "negative"]
                    }
                },
                "required": ["title", "key_points", "sentiment"]
            }
        }
    }
)

# 响应保证匹配您的模式
import json
facts = json.loads(message.content[0].text)
```

</Tab>
<Tab value="cURL">

```bash
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gemini-2.0-flash-exp",
    "messages": [
      {"role": "user", "content": "从这篇文章中提取关键事实..."}
    ],
    "response_format": {
      "type": "json_schema",
      "json_schema": {
        "name": "article_facts",
        "strict": true,
        "schema": {
          "type": "object",
          "properties": {
            "title": {"type": "string"},
            "author": {"type": "string"},
            "key_points": {
              "type": "array",
              "items": {"type": "string"}
            },
            "sentiment": {
              "type": "string",
              "enum": ["positive", "neutral", "negative"]
            }
          },
          "required": ["title", "key_points", "sentiment"]
        }
      }
    }
  }'
```

</Tab>
</Tabs>

<Callout type="info">
**协议自动检测。** BitRouter 自动检测您使用的协议（OpenAI Chat、Anthropic Messages 等）并将您的模式转换为提供商的原生格式。
</Callout>

## 工作原理

BitRouter 提供了一个跨所有提供商工作的统一 `response_format` 字段。当您发送请求时：

1. **入站解析** — BitRouter 检测您的客户端协议并提取模式
2. **跨协议路由** — 将模式转换为上游提供商的原生格式
3. **响应流式传输** — 通过标准 `TextDelta` 事件流式传输部分 JSON
4. **格式保留** — 响应保持您请求的协议形状

这意味着 OpenAI 客户端可以路由到 Anthropic，或者 Anthropic 客户端可以路由到 Google Gemini — 转换是自动进行的。

## 协议格式

每个提供商都有自己的原生结构化输出格式。BitRouter 在它们之间进行转换：

| 协议 | 原生格式 | BitRouter 接受 | 备注 |
| --- | --- | --- | --- |
| **OpenAI Chat** | `response_format.json_schema` | 相同 | 完全支持 `name`、`strict`、`schema` |
| **OpenAI Responses** | `text.format` | 相同 | 保留兄弟键如 `text.verbosity` |
| **Anthropic** | `output_config.format` | 也支持旧版 `output_format` | 没有 `name` 或 `strict` 字段 |
| **Google Gemini** | `generationConfig` | `responseMimeType` + `responseSchema` | 仅模式，无 name/strict |

### 字段映射

在协议之间路由时，BitRouter 处理这些转换：

- **`name`** — OpenAI 必需，如果未提供则默认为 `"response"`。对于 Anthropic/Google 会被删除。
- **`strict`** — OpenAI 的严格模式标志。对于不支持它的提供商会被删除。
- **`schema`** — JSON Schema 本身。通过特定于提供商的调整传递。

<Callout type="warning">
**不执行验证。** BitRouter 是代理，而不是验证器。它将您的模式传递给提供商并按原样返回其响应。提供商负责模式执行。
</Callout>

## 高级用法

### 复杂嵌套模式

结构化输出支持具有嵌套对象、数组和枚举的任意复杂 JSON 模式：

```typescript
const schema = {
  type: 'object',
  properties: {
    analysis: {
      type: 'object',
      properties: {
        entities: {
          type: 'array',
          items: {
            type: 'object',
            properties: {
              name: { type: 'string' },
              type: { 
                type: 'string',
                enum: ['person', 'organization', 'location', 'event'] 
              },
              confidence: { 
                type: 'number',
                minimum: 0,
                maximum: 1
              },
              relationships: {
                type: 'array',
                items: {
                  type: 'object',
                  properties: {
                    target: { type: 'string' },
                    type: { type: 'string' }
                  },
                  required: ['target', 'type']
                }
              }
            },
            required: ['name', 'type', 'confidence']
          }
        },
        summary: { type: 'string', maxLength: 500 },
        topics: {
          type: 'array',
          items: { type: 'string' },
          minItems: 1,
          maxItems: 5
        }
      },
      required: ['entities', 'summary', 'topics']
    },
    metadata: {
      type: 'object',
      properties: {
        processed_at: { type: 'string', format: 'date-time' },
        confidence_score: { type: 'number' }
      },
      required: ['processed_at']
    }
  },
  required: ['analysis', 'metadata']
};
```

### 流式结构化输出

结构化输出适用于流式响应。部分 JSON 作为文本增量流式传输回来：

```typescript
const stream = await client.chat.completions.create({
  model: 'claude-3-5-sonnet-latest',
  messages: [{ role: 'user', content: '分析此文档...' }],
  response_format: {
    type: 'json_schema',
    json_schema: { name: 'analysis', schema: schema }
  },
  stream: true
});

let buffer = '';
for await (const chunk of stream) {
  const delta = chunk.choices[0]?.delta?.content;
  if (delta) {
    buffer += delta;
    // 部分 JSON 累积：{"analysis":{"entities":[{"name":"Acme Corp"...
  }
}

const result = JSON.parse(buffer);
```

### 提供商特定注意事项

#### OpenAI
- 需要 `name` 字段（BitRouter 提供 `"response"` 作为默认值）
- 支持 `strict: true` 以实现更严格的模式执行
- 适用于 GPT-4o 模型（2024 年 11 月+）及更新版本

#### Anthropic
- 通过 `output_config.format` 原生支持（不需要测试版标头）
- 不支持 `name` 或 `strict` 字段（BitRouter 会删除它们）
- 适用于 Claude 3.5 Sonnet 及更新版本

#### Google Gemini
- 使用 `generationConfig.responseMimeType: "application/json"`
- 模式放在 `generationConfig.responseSchema` 中
- 适用于 Gemini 1.5 Pro 和 Flash 模型

#### 自定义提供商
在 BitRouter 的[提供商注册表](/docs/guides/register-as-a-provider)中注册的任何提供商都可以通过我们的统一模式格式支持结构化输出。

## 限制

### 模型要求
BitRouter Cloud 上所有领先的智能体 LLM 都支持结构化输出，包括：

- **OpenAI**：GPT-4o、GPT-4o-mini、o1、o1-mini、o3-mini
- **Anthropic**：Claude 3.5 Sonnet、Claude 3.5 Haiku、Claude 3 Opus
- **Google**：Gemini 2.0 Flash、Gemini 1.5 Pro、Gemini 1.5 Flash
- **DeepSeek**：DeepSeek V3、DeepSeek R1
- **Meta**：Llama 3.3 70B、Llama 3.1 405B
- **其他**：大多数现代模型都支持 JSON 模式

### 模式约束
- 最大模式大小因提供商而异（通常为 10-50KB）
- 某些提供商限制嵌套深度（通常为 10-20 级）
- 某些 JSON Schema 功能可能不受支持（例如 `$ref`、`patternProperties`）

### 不支持
- **响应验证** — BitRouter 不会根据模式验证响应
- **模式转换** — 输入必须是有效的 JSON Schema，不支持 TypeScript/Zod 等
- **旧版 JSON 模式** — 单独使用 `response_format: { type: "json_object" }`

<Callout>
**错误处理。** 如果模型不支持结构化输出，您将收到 `400 Bad Request` 以及有关不兼容的详细信息。如果模式无效，提供商将返回其特定的错误消息。
</Callout>

## 迁移指南

如果您当前正在使用特定于提供商的结构化输出 API，以下是迁移方法：

### 从 OpenAI
无需更改 — BitRouter 接受相同的格式：
```typescript
// 按原样工作
response_format: {
  type: 'json_schema',
  json_schema: { name: 'my_schema', strict: true, schema: {...} }
}
```

### 从 Anthropic
更新为使用统一格式：
```python
# 之前（Anthropic 特定）
output_config={"format": {"type": "json_schema", "schema": {...}}}

# 之后（BitRouter 统一）
response_format={
  "type": "json_schema",
  "json_schema": {"schema": {...}}  # name 和 strict 是可选的
}
```

### 从 Google Gemini
从拆分字段切换到统一格式：
```javascript
// 之前（Gemini 特定）
generationConfig: {
  responseMimeType: 'application/json',
  responseSchema: {...}
}

// 之后（BitRouter 统一）
response_format: {
  type: 'json_schema',
  json_schema: { schema: {...} }
}
```

## 另请参阅

- [模型发现](/docs/reference/discovery/listModels) — 检查哪些模型支持结构化输出
- [提供商选择](/docs/features/provider-selection) — 路由到支持结构化输出的提供商
- [API 参考](/docs/reference) — 完整的 API 文档
