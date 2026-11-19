import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

export const TEXT = 'Synthetic gateway response ☀\n';
export const STREAM_CHUNKS = 100;
export const TOOLS = [{ type: 'function', function: {
  name: 'add', description: 'Add two integers',
  parameters: { type: 'object', properties: { left: { type: 'integer' }, right: { type: 'integer' } }, required: ['left', 'right'] },
} }];
export const ARGUMENTS = { left: 19, right: 23 };
export const TOOL_ID = 'call_synthetic_add';
export const RESULT = '42';

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  assert.equal(typeof process.send, 'function', 'synthetic peer requires an IPC parent');
  const counters = { started: 0, completed: 0, cancelled: 0, failed: 0, active: 0 };
  const server = createServer(async (request, response) => {
    counters.started++;
    counters.active++;
    const abort = new AbortController();
    response.once('close', () => {
      if (!response.writableFinished) abort.abort();
    });
    try {
      assert.equal(request.method, 'POST');
      assert.equal(request.url, '/v1/chat/completions');
      assert.equal(request.httpVersion, '1.1');
      const chunks = [];
      let bytes = 0;
      for await (const chunk of request) {
        bytes += chunk.length;
        assert.ok(bytes <= 64 * 1024);
        chunks.push(chunk);
      }
      const body = JSON.parse(Buffer.concat(chunks));
      const model = body.model;
      assert.ok(['short', 'paced', 'tools'].includes(model));
      const call = { index: 0, id: TOOL_ID, type: 'function', function: {
        name: 'add', arguments: JSON.stringify(ARGUMENTS),
      } };
      const usage = { prompt_tokens: 3, completion_tokens: 5, total_tokens: 8 };
      const result = { id: 'chatcmpl-fixture', created: 1, model };
      if (!body.stream) {
        response.writeHead(200, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ ...result, object: 'chat.completion', usage,
          choices: [{ index: 0, message: { role: 'assistant', content: TEXT }, finish_reason: 'stop' }],
        }));
      } else {
        const toolResult = body.messages.find(message => message.role === 'tool');
        const calling = model === 'tools' && !toolResult;
        if (model === 'tools') {
          assert.equal(body.tools[0].function.name, 'add');
          if (!calling) {
            const prior = body.messages.find(message => message.role === 'assistant');
            assert.deepEqual(JSON.parse(prior.tool_calls[0].function.arguments), ARGUMENTS);
            assert.equal(toolResult.tool_call_id, TOOL_ID);
            assert.equal(toolResult.content, RESULT);
          }
        }
        const send = (delta, finish_reason = null) => response.write('data: ' + JSON.stringify({
          ...result, object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason }],
        }) + '\n\n');
        response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
        send({ role: 'assistant' });
        if (calling) send({ tool_calls: [call] });
        else if (model === 'paced') {
          for (let index = 0; index < STREAM_CHUNKS; index++) {
            await delay(100, undefined, { signal: abort.signal });
            send({ content: TEXT });
          }
        } else send({ content: model === 'tools' ? RESULT : TEXT });
        send({}, calling ? 'tool_calls' : 'stop');
        response.end('data: ' + JSON.stringify({ ...result, object: 'chat.completion.chunk', choices: [], usage }) +
          '\n\ndata: [DONE]\n\n');
      }
      counters.completed++;
    } catch {
      if (abort.signal.aborted) counters.cancelled++;
      else {
        counters.failed++;
        response.destroy();
      }
    } finally {
      counters.active--;
    }
  });
  server.listen(0, '127.0.0.1', () => process.send({ port: server.address().port }));
  process.on('message', () => process.send({ counters }));
  process.on('disconnect', () => {
    server.close();
    server.closeAllConnections();
  });
}
