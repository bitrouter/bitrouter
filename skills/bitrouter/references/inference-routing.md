# Local provider scheduling

`inference-routing` is an independent optional build feature. It schedules among
providers of the selected model; each provider owns its own replicas. The default
is disabled. A gateway without this feature rejects `enabled: true` before work.

Keep all eligible (provider, model, exact endpoint, account label) tuples mapped:

```yaml
inference_routing:
  enabled: true
  policy: load_aware # or prefix_affinity for private approximate observations
  destinations:
    - id: local-model
      provider: local
      model: model-one
      endpoint: http://127.0.0.1:8000/v1
      max_active: 8
```

The provider must already exist under `providers`; this section neither installs
providers nor supplies credentials. In ordinary mode each declaration must resolve
at startup. Tenant mode resolves retained membership/credentials per request.
Any unmapped eligible target refuses dispatch. Use local gateway state. Shared
mode, managed native roots and server-tool loops are unsupported for scheduling.

Save configuration, then use `bro reload`. Hot fields are enabled, policy,
tenant_weights, queue.wait_timeout_ms, queue.request_timeout_ms and prefix.ttl_ms.
Other scheduler settings require restart; mixing them into a hot change rejects
the whole reload. Existing queued requests retain their strategy and deadline.
Disable bypasses new scheduling while old work drains; re-enable reuses the same
capacity/uncertainty pool. Process restart starts approximate affinity cold.
These limits are local observations, not physical provider capacity guarantees.

For two gateways, put semantic selection on the user gateway and provider
scheduling on the serving gateway. Configure one serving hop on the user gateway
so only the serving gateway owns provider fallback. Preserve the selected model
and generation constraints. Each gateway authenticates independently.

Ordinary generation clients can send `x-bitrouter-timeout-ms: 2000`. To relay a
shrinking budget, explicitly add this rule only to the gateway provider:

```yaml
headers:
  x-bitrouter-timeout-ms: {passthrough: true}
```

The header is not passed to ordinary providers by default. It conveys no tenant
or execution authority. Older gateways may ignore it; it does not prove upstream
cancellation. JSON work continues settlement on disconnect; SSE drops and sent
request timeouts retain conservative uncertainty. Managed execution uses its own
deadline contract and rejects this header.
