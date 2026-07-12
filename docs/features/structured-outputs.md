---
title: Structured Outputs
description: Enforce JSON schemas across all providers — automatic protocol translation for OpenAI, Anthropic, Google, and more.
sourceHash: 6a4771c946e6a64814738de8566a6afa1971681ee5d7ad0c72b25ed12359ec59
---

**Structured outputs** guarantee that model responses conform to your exact JSON schema. BitRouter provides a unified API that works seamlessly across all major providers — automatically translating between each provider's native format.

## Quick start

Define your schema once, use it everywhere. BitRouter handles the protocol translation:

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
    { role: 'user', content: 'Extract the key facts from this article...' }
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

// Response is guaranteed to match your schema
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
        {"role": "user", "content": "Extract the key facts from this article..."}
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

# Response is guaranteed to match your schema
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
      {"role": "user", "content": "Extract the key facts from this article..."}
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
**Protocol auto-detection.** BitRouter automatically detects which protocol you're using (OpenAI Chat, Anthropic Messages, etc.) and translates your schema to the provider's native format.
</Callout>

## How it works

BitRouter provides a unified `response_format` field that works across all providers. When you send a request:

1. **Inbound parsing** — BitRouter detects your client protocol and extracts the schema
2. **Cross-protocol routing** — The schema is translated to the upstream provider's native format
3. **Response streaming** — Partial JSON streams back via standard `TextDelta` events
4. **Format preservation** — Responses maintain your requested protocol's shape

This means an OpenAI client can route to Anthropic, or an Anthropic client can route to Google Gemini — the translation happens automatically.

## Protocol formats

Each provider has its own native structured output format. BitRouter translates between them:

| Protocol | Native format | BitRouter accepts | Notes |
| --- | --- | --- | --- |
| **OpenAI Chat** | `response_format.json_schema` | Same | Full support for `name`, `strict`, `schema` |
| **OpenAI Responses** | `text.format` | Same | Preserves sibling keys like `text.verbosity` |
| **Anthropic** | `output_config.format` | Also legacy `output_format` | No `name` or `strict` fields |
| **Google Gemini** | `generationConfig` | `responseMimeType` + `responseSchema` | Schema only, no name/strict |

### Field mappings

When routing between protocols, BitRouter handles these transformations:

- **`name`** — Required by OpenAI, defaults to `"response"` if not provided. Dropped for Anthropic/Google.
- **`strict`** — OpenAI's strict mode flag. Dropped for providers that don't support it.
- **`schema`** — The JSON Schema itself. Passed through with provider-specific adjustments.

<Callout type="warning">
**No validation performed.** BitRouter is a proxy, not a validator. It passes your schema to the provider and returns their response as-is. The provider is responsible for schema enforcement.
</Callout>

## Advanced usage

### Complex nested schemas

Structured outputs support arbitrarily complex JSON schemas with nested objects, arrays, and enums:

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

### Streaming structured outputs

Structured outputs work with streaming responses. Partial JSON is streamed back as text deltas:

```typescript
const stream = await client.chat.completions.create({
  model: 'claude-3-5-sonnet-latest',
  messages: [{ role: 'user', content: 'Analyze this document...' }],
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
    // Partial JSON accumulates: {"analysis":{"entities":[{"name":"Acme Corp"...
  }
}

const result = JSON.parse(buffer);
```

### Provider-specific considerations

#### OpenAI
- Requires a `name` field (BitRouter supplies `"response"` as default)
- Supports `strict: true` for stricter schema enforcement
- Works with GPT-4o models (Nov 2024+) and newer

#### Anthropic
- Native support via `output_config.format` (no beta headers needed)
- Doesn't support `name` or `strict` fields (BitRouter drops them)
- Works with Claude 3.5 Sonnet and newer

#### Google Gemini
- Uses `generationConfig.responseMimeType: "application/json"`
- Schema goes in `generationConfig.responseSchema`
- Works with Gemini 1.5 Pro and Flash models

#### Custom providers
Any provider registered in BitRouter's [provider registry](/docs/guides/register-as-a-provider) can support structured outputs through our unified schema format.

## Limitations

### Model requirements
All leading agentic LLMs available on BitRouter Cloud support structured outputs, including:

- **OpenAI**: GPT-4o, GPT-4o-mini, o1, o1-mini, o3-mini
- **Anthropic**: Claude 3.5 Sonnet, Claude 3.5 Haiku, Claude 3 Opus
- **Google**: Gemini 2.0 Flash, Gemini 1.5 Pro, Gemini 1.5 Flash
- **DeepSeek**: DeepSeek V3, DeepSeek R1
- **Meta**: Llama 3.3 70B, Llama 3.1 405B
- **Others**: Most modern models support JSON schemas

### Schema constraints
- Maximum schema size varies by provider (typically 10-50KB)
- Some providers limit nesting depth (usually 10-20 levels)
- Certain JSON Schema features may not be supported (e.g., `$ref`, `patternProperties`)

### Not supported
- **Response validation** — BitRouter doesn't validate responses against schemas
- **Schema conversion** — Input must be valid JSON Schema, no TypeScript/Zod/etc.
- **Legacy JSON mode** — Use `response_format: { type: "json_object" }` separately

<Callout>
**Error handling.** If a model doesn't support structured outputs, you'll receive a `400 Bad Request` with details about the incompatibility. If the schema is invalid, the provider will return their specific error message.
</Callout>

## Migration guide

If you're currently using provider-specific structured output APIs, here's how to migrate:

### From OpenAI
No changes needed — BitRouter accepts the same format:
```typescript
// Works as-is
response_format: {
  type: 'json_schema',
  json_schema: { name: 'my_schema', strict: true, schema: {...} }
}
```

### From Anthropic
Update to use the unified format:
```python
# Before (Anthropic-specific)
output_config={"format": {"type": "json_schema", "schema": {...}}}

# After (BitRouter unified)
response_format={
  "type": "json_schema",
  "json_schema": {"schema": {...}}  # name and strict are optional
}
```

### From Google Gemini
Switch from split fields to unified format:
```javascript
// Before (Gemini-specific)
generationConfig: {
  responseMimeType: 'application/json',
  responseSchema: {...}
}

// After (BitRouter unified)
response_format: {
  type: 'json_schema',
  json_schema: { schema: {...} }
}
```

## See also

- [Model discovery](/docs/reference/discovery/listModels) — Check which models support structured outputs
- [Provider selection](/docs/features/provider-selection) — Route to providers with structured output support
- [API Reference](/docs/reference) — Complete API documentation
