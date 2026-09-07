// Application-owned evidence transport. Native input and model options are unchanged.
// ACP extensions: https://agentclientprotocol.com/protocol/v1/extensibility
export const metadataKey = "bitrouter/native-evidence";
export const notificationMethod = "_bitrouter/nativeBinding";
const spans = new WeakMap();
const maxObservations = 128;

export async function initialize(invoke, adapter) {
  const result = await invoke();
  return { ...result, _meta: { ...result._meta, [metadataKey]: { schema: 1, adapter } } };
}

function emit(span, event) {
  if (span.sequence >= maxObservations) {
    span.failures += 1;
    return;
  }
  const payload = {
    schema: 1,
    sessionId: span.params.sessionId,
    origin: span.origin,
    adapter: span.adapter,
    sequence: span.sequence++,
    event,
  };
  // Observations use the adapter's existing writer. Never write protocol bytes
  // directly to stdout, wait for a controller acknowledgment, or throw into a
  // native acceptance callback. Missing sequence numbers remain evidence gaps.
  try {
    const pending = Promise.resolve(span.notify(payload)).catch(() => {
      span.failures += 1;
    });
    span.pending.push(pending);
  } catch {
    span.failures += 1;
  }
}

export function observe(params, event) {
  const span = spans.get(params);
  if (span) emit(span, event);
}

export async function prompt(params, invoke, notify, adapter) {
  const claim = params?._meta?.[metadataKey];
  if (!claim || typeof claim !== "object" || Array.isArray(claim)) {
    return invoke(params);
  }
  const clean = { ...params, _meta: { ...params._meta } };
  delete clean._meta[metadataKey];
  if (claim.metaState === "absent") delete clean._meta;
  if (claim.metaState === "null") clean._meta = null;
  const span = {
    params: clean, origin: claim.origin, adapter, notify,
    sequence: 0, failures: 0, pending: [],
  };
  spans.set(clean, span);
  emit(span, { kind: "started" });
  let outcome = "threw";
  try {
    const result = await invoke(clean);
    outcome = "returned";
    return result;
  } finally {
    emit(span, { kind: "finished", outcome, notificationFailures: span.failures });
    // Bound local transport draining. The original callback closure retains
    // its span for a late acceptance after cancellation; it never consults the
    // session's latest prompt. Finished is an ACP boundary, not quiescence.
    let timer;
    try {
      await Promise.race([
        Promise.allSettled(span.pending),
        new Promise((resolve) => { timer = setTimeout(resolve, 1000); }),
      ]);
    } finally {
      clearTimeout(timer);
      span.pending = [];
    }
  }
}
