import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { Agent, request } from 'node:http';
import { connect, constants } from 'node:http2';
import { performance } from 'node:perf_hooks';
import { TEXT, STREAM_CHUNKS, TOOLS, ARGUMENTS, TOOL_ID, RESULT } from './synthetic-peer.mjs';

export const COUNTS = { json: 128, instant: 128, tools: 100, long: 20, cancel: 100 };
export const CONCURRENCY = 4;
const MESSAGES = [{ role: 'user', content: 'Run the synthetic fixture.' }];

export function client(origin, protocol, headers, prefix = '') {
  const agent = new Agent({ keepAlive: true, maxSockets: CONCURRENCY });
  const session = protocol === 'h2c' ? connect(origin) : null;
  session?.on('error', () => {});
  async function send(scenario, messages, leg = 'reply') {
    const started = performance.now();
    const correlation = randomUUID();
    const long = scenario === 'long' || scenario === 'cancel';
    const stream = scenario !== 'json';
    const model = prefix + (scenario === 'tools' ? 'tools' : long ? 'paced' : 'short');
    const data = JSON.stringify({ model, stream, messages,
      ...(stream ? { stream_options: { include_usage: true } } : {}),
      ...(scenario === 'tools' ? { tools: TOOLS } : {}),
    });
    const fields = { ...headers, 'content-type': 'application/json', 'content-length': Buffer.byteLength(data),
      ...(headers.authorization ? { 'bitrouter-correlation-id': correlation } : {}),
    };
    const signal = AbortSignal.timeout(25_000);
    let response, version, status, received, cancel;
    if (session) {
      response = session.request({ ':method': 'POST', ':path': '/v1/chat/completions', ...fields }, { signal });
      [received] = await new Promise((resolve, reject) => {
        response.once('response', (...values) => resolve(values));
        response.once('error', reject);
        response.end(data);
      });
      version = '2.0';
      status = received[':status'];
      cancel = () => response.close(constants.NGHTTP2_CANCEL);
    } else {
      response = await new Promise((resolve, reject) => {
        const outgoing = request(origin + '/v1/chat/completions', { method: 'POST', agent, headers: fields, signal }, resolve);
        outgoing.once('error', reject);
        outgoing.end(data);
      });
      received = response.headers;
      version = response.httpVersion;
      status = response.statusCode;
      cancel = () => response.destroy();
    }
    assert.equal(status, 200, `${scenario}/${leg} HTTP status`);
    assert.equal(version, session ? '2.0' : '1.1');
    const row = { scenario, leg, version, correlation, execution: received['x-bitrouter-request-id'],
      elapsed: 0, first: null, cancel: false };
    const finish = () => ({ ...row, elapsed: performance.now() - started });
    if (!stream) {
      assert.match(received['content-type'], /^application\/json/);
      const parts = [];
      let bytes = 0;
      for await (const part of response) {
        bytes += part.length;
        assert.ok(bytes <= 64 * 1024);
        parts.push(part);
      }
      const value = JSON.parse(Buffer.concat(parts));
      assert.equal(value.choices[0].message.content, TEXT);
      assert.equal(value.choices[0].finish_reason, 'stop');
      checkUsage(value.usage);
      return finish();
    }
    assert.match(received['content-type'], /^text\/event-stream/);
    const decoder = new TextDecoder('utf-8', { fatal: true });
    let pending = '', output = '', finishReason = null, done = 0, events = 0, usage, bytes = 0;
    const calls = new Map();
    for await (const part of response) {
      bytes += part.length;
      assert.ok(bytes <= 256 * 1024, 'synthetic stream exceeds its bound');
      pending += decoder.decode(part, { stream: true }).replaceAll('\r\n', '\n');
      let boundary;
      while ((boundary = pending.indexOf('\n\n')) !== -1) {
        const frame = pending.slice(0, boundary);
        pending = pending.slice(boundary + 2);
        const data = frame.split('\n').filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n');
        if (!data) continue;
        assert.equal(done, 0, 'payload after terminal event');
        if (data === '[DONE]') {
          done++;
          continue;
        }
        const value = JSON.parse(data);
        assert.ok(!value.error && Array.isArray(value.choices));
        if (value.usage) usage = value.usage;
        for (const choice of value.choices) {
          const delta = choice.delta ?? {};
          if (delta.content) {
            row.first ??= performance.now() - started;
            output += delta.content;
          }
          for (const tool of delta.tool_calls ?? []) {
            const call = calls.get(tool.index) ?? { id: '', type: 'function', function: { name: '', arguments: '' } };
            call.id ||= tool.id ?? '';
            call.function.name += tool.function?.name ?? '';
            call.function.arguments += tool.function?.arguments ?? '';
            calls.set(tool.index, call);
            if (tool.function?.name || tool.function?.arguments) row.first ??= performance.now() - started;
          }
          if (choice.finish_reason) finishReason = choice.finish_reason;
        }
        if (++events === 3 && scenario === 'cancel') {
          assert.ok(row.first !== null && finishReason === null);
          row.cancel = true;
          cancel();
          return finish();
        }
      }
    }
    pending += decoder.decode();
    assert.equal(pending.trim(), '');
    assert.ok(row.first !== null && scenario !== 'cancel');
    assert.equal(done, 1);
    checkUsage(usage);
    if (leg === 'call') {
      assert.equal(finishReason, 'tool_calls');
      assert.equal(output, '');
      assert.equal(calls.size, 1);
      const call = calls.get(0);
      assert.equal(call.id, TOOL_ID);
      assert.equal(call.function.name, 'add');
      assert.deepEqual(JSON.parse(call.function.arguments), ARGUMENTS);
      return { ...finish(), tool: call };
    }
    assert.equal(finishReason, 'stop');
    assert.equal(calls.size, 0);
    assert.equal(output, long ? TEXT.repeat(STREAM_CHUNKS) : scenario === 'tools' ? RESULT : TEXT);
    if (long) assert.ok(performance.now() - started >= 9_500, 'paced SSE finished too early');
    return finish();
  }
  return {
    async conversation(scenario) {
      if (scenario !== 'tools') return [await send(scenario, MESSAGES)];
      const { tool, ...call } = await send(scenario, MESSAGES, 'call');
      const arguments_ = JSON.parse(tool.function.arguments);
      const messages = [...MESSAGES, { role: 'assistant', content: null, tool_calls: [tool] },
        { role: 'tool', tool_call_id: tool.id, content: String(arguments_.left + arguments_.right) },
      ];
      return [call, await send(scenario, messages)];
    },
    close() { agent.destroy(); session?.destroy(); },
  };
}

function checkUsage(usage) {
  assert.deepEqual([usage?.prompt_tokens, usage?.completion_tokens, usage?.total_tokens], [3, 5, 8]);
}
