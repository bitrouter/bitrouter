---
title: 外部供应商（BYOK）
description: 自带供应商密钥——使用你自己的账号，直接向供应商付费，BitRouter 路由请求时不抽成、不加价。支持自托管、本地运行和 BitRouter Cloud。
sourceHash: db475e399f3b5f33095b1ac876df326a5cfe0654d33aff75b1583070724779f1
---

**BYOK**（Bring Your Own Key，自带密钥）让你的请求走你自己的供应商账号，而不是 BitRouter 的。你按上游的标价直接付费——BitRouter 不抽成、不加每 token 费用，且从不以明文持有你的密钥。

BYOK 在各种部署方式下的行为一致：

- **自托管 / 本地二进制** — 在环境中设置供应商密钥，BitRouter 启动时自动检测，无需上传或加密步骤。
- **BitRouter Cloud** — 密钥在客户端用 sealed-box 加密后再提交，节点始终只看到密文。

## 本地模式——环境变量自动检测

当你自托管或在本地运行二进制时，没有上传步骤：把供应商密钥设到环境里就完事了。无需配置文件。

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GOOGLE_API_KEY=AIza...
bitrouter
```

BitRouter 在启动时检测密钥，仅暴露当前已配置密钥的供应商。当某供应商的密钥缺失时，路由到该供应商的请求会返回 `402 Payment Required`，错误体里会指明缺失的环境变量名。

### 识别的环境变量

| 供应商 | 推荐名 | 兼容名 |
| --- | --- | --- |
| OpenAI | `BITROUTER_OPENAI_API_KEY` | `OPENAI_API_KEY` |
| Anthropic | `BITROUTER_ANTHROPIC_API_KEY` | `ANTHROPIC_API_KEY` |
| Google | `BITROUTER_GOOGLE_API_KEY` | `GOOGLE_API_KEY`、`GEMINI_API_KEY` |
| 自定义（registry 内） | `BITROUTER_<PROVIDER_ID>_API_KEY` | — |

`BITROUTER_*` 名称的优先级高于兼容名——当你的 shell 已经为别的工具设置了 `OPENAI_API_KEY`，又想让 BitRouter 走另一个账号时很有用。

对于在 [registry](/docs/guides/register-as-a-provider) 中以 `id: my-provider` 注册的自定义供应商，请设置 `BITROUTER_MY_PROVIDER_API_KEY`（全大写，连字符变下划线）。

<Callout type="info">
**密钥可热更新。** BitRouter 监听父进程的环境；更新密钥后（重新 export 并 `kill -HUP $(pgrep bitrouter)`），下一次请求即生效，无需重启。
</Callout>

## 在 BitRouter Cloud 上

在 `cloud.bitrouter.ai` 上，你的供应商密钥会在**客户端**用节点的 X25519 sealed-box 公钥加密之后再提交。节点在写入路径上从不看到明文；密文仅在请求时于内存中解密、且永不落日志。

流程如下：

1. **拉取节点公钥。** [`GET /v1/byok/encryption-pubkey`](/docs/reference/byok/getEncryptionPubkey) 返回当前的 X25519 公钥及其 `kek_id` 指纹。按指纹缓存，并以 `If-None-Match` 形式回传，可在密钥未变时直接拿到 `304 Not Modified`。
2. **用公钥加密明文密钥。** 使用 libsodium 的 `crypto_box_seal`（或任何 sealed-box 实现）。
3. **提交密文。** [console](https://cloud.bitrouter.ai) 在你粘贴密钥时，就在浏览器里完成第 1、2 步——你不需要离开 dashboard。同样的提交 API 也可用于脚本化的入网；详见 [API 参考](/docs/reference/byok/getEncryptionPubkey)。

### 加密示例

如果你用脚本提交密钥，下面是最小化的 sealed-box 步骤。其输出的密文就是提交端点期望的内容。

<Tabs items={['Node.js', 'Python', 'Shell']}>
<Tab value="Node.js">

```js
import sodium from 'libsodium-wrappers';

await sodium.ready;

const meta = await fetch(
  'https://cloud.bitrouter.ai/v1/byok/encryption-pubkey'
).then(r => r.json());

const ciphertext = sodium.crypto_box_seal(
  sodium.from_string(process.env.OPENAI_API_KEY),
  sodium.from_base64(meta.public_key, sodium.base64_variants.ORIGINAL)
);

const ciphertextB64 = sodium.to_base64(ciphertext, sodium.base64_variants.ORIGINAL);
// 提交 { provider_name: 'openai', kek_id: meta.kek_id, ciphertext_b64: ciphertextB64, key_prefix: 'sk-...' }
```

</Tab>
<Tab value="Python">

```python
import os, base64, requests
from nacl.public import PublicKey, SealedBox

meta = requests.get('https://cloud.bitrouter.ai/v1/byok/encryption-pubkey').json()
pubkey = PublicKey(base64.b64decode(meta['public_key']))
ciphertext = SealedBox(pubkey).encrypt(os.environ['OPENAI_API_KEY'].encode())

payload = {
    'provider_name': 'openai',
    'kek_id': meta['kek_id'],
    'ciphertext_b64': base64.b64encode(ciphertext).decode(),
    'key_prefix': 'sk-...',
}
# 将 `payload` POST 到 BYOK 提交端点。
```

</Tab>
<Tab value="Shell">

```bash
META=$(curl -s https://cloud.bitrouter.ai/v1/byok/encryption-pubkey)
PUBKEY=$(echo "$META" | jq -r .public_key)
KEK_ID=$(echo "$META" | jq -r .kek_id)

CIPHERTEXT=$(printf '%s' "$OPENAI_API_KEY" \
  | sodium-seal --pubkey "$PUBKEY" \
  | base64)

# 提交 { provider_name: "openai", kek_id: "$KEK_ID", ciphertext_b64: "$CIPHERTEXT", key_prefix: "sk-..." }
```

</Tab>
</Tabs>

公钥端点支持 `If-None-Match` 缓存——把上一次响应中的 `kek_id` 钉住后回传，密钥未变时即可拿到 `304 Not Modified`。

### 密钥作用域

密钥的作用域是你的**用户账号**——你账号下签发的所有 API key 和 OAuth token 均可通过你存储的密钥路由请求。任何人——包括 BitRouter 运维——都无法读取密文或原始密钥。

### 轮换、撤销与审计

- **轮换** — 对同一供应商提交一份新密文即可，旧记录会原子性地被覆盖。
- **撤销** — 在 dashboard 里操作。使用旧密钥的在途请求会跑完；新请求开始返回 `402 Payment Required`。
- **审计** — 每一次提交都会记录提交时间和所用 `kek_id`。任何人——包括 BitRouter 运维——都不可见明文。

当节点的 `kek_id` 轮换时（每 90 天一次），旧密钥会被保留在内存中，已提交的密文在请求时仍可正常解密。新提交必须使用当前的 `kek_id`；只有在你明确希望迁移到新密钥时，才需要重新加密并提交。

## 自定义供应商

只要 [registry](/docs/guides/register-as-a-provider) 中的供应商在其 manifest 的 `payment.modes` 里声明了 `byok`，就可以走 BYOK。提交时使用的 provider 字段必须与 registry 里的 `id` 完全一致。本地模式的环境变量识别遵循 `BITROUTER_<PROVIDER_ID>_API_KEY` 约定。
