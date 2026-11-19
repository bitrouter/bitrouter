import assert from 'node:assert/strict';
import { execFile, fork, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { connect, createServer } from 'node:net';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';
import { client, CONCURRENCY, COUNTS } from './synthetic-client.mjs';
import { createTenant } from './synthetic-tenant.mjs';

const execute = promisify(execFile);
const providerFile = new URL('./synthetic-peer.mjs', import.meta.url);
export const LANES = {
  'minimum-http1': ['direct', 'gateway', 'tenant'],
  'http2-http1': ['direct', 'gateway', 'tenant'],
  'http2-h2c': ['gateway', 'tenant'],
};
const until = async (deadline, check) => {
  while (performance.now() < deadline) {
    if (await check()) return;
    await delay(25);
  }
  throw Error('Timed out waiting for owned fixture');
};

async function reserve() {
  const server = createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return server;
}

async function status(path) {
  return new Promise((resolve, reject) => {
    const socket = connect(path);
    let value = '';
    socket.setTimeout(250, () => socket.destroy(Error('Control socket timeout')));
    socket.once('error', reject);
    socket.once('connect', () => socket.write('{"cmd":"status"}\n'));
    socket.on('data', chunk => {
      value += chunk.toString();
      const line = value.indexOf('\n');
      if (line !== -1) {
        socket.end();
        try { resolve(JSON.parse(value.slice(0, line))); } catch (error) { reject(error); }
      }
    });
    socket.on('end', () => reject(Error('Incomplete control status')));
  });
}

async function stop(child) {
  if (!child || child.pid === undefined || child.exitCode !== null || child.signalCode !== null) return;
  const ended = once(child, 'exit');
  child.kill('SIGTERM');
  const deadline = new AbortController();
  try {
    const timeout = await Promise.race([ended.then(() => false), delay(4000, true, { signal: deadline.signal })]);
    if (timeout) {
      child.kill('SIGKILL');
      await ended;
    }
  } finally { deadline.abort(); }
}

async function memory(pid) {
  const { stdout } = await execute('ps', ['-o', 'rss=', '-p', String(pid)], { timeout: 3000 });
  const kib = Number(stdout.trim());
  assert.ok(Number.isFinite(kib) && kib > 0, 'missing RSS sample');
  return kib;
}

async function window(driver, scenario, count, concurrency) {
  const started = performance.now();
  let cursor = 0;
  let firstError;
  const groups = Array(count);
  await Promise.all(Array.from({ length: Math.min(concurrency, count) }, async () => {
    while (cursor < count && !firstError) {
      const index = cursor++;
      try { groups[index] = await driver.conversation(scenario); }
      catch (error) { firstError ??= error; }
    }
  }));
  if (firstError) throw firstError;
  return { elapsed: performance.now() - started, rows: groups.flat() };
}

export async function trial(binary, lane, protocol, smoke, counts = COUNTS) {
  const temp = await mkdtemp('/tmp/br-synth-');
  let gateway, provider, driver, timer, sampleError, pendingSample;
  const processSamples = [];
  const controlKey = randomBytes(32).toString('hex');
  const children = new Set();
  const abort = () => {
    for (const child of children) child.kill('SIGTERM');
  };
  const runEnv = { PATH: '/usr/bin:/bin', HOME: temp, XDG_CONFIG_HOME: temp, BITROUTER_HOME: temp, TMPDIR: temp,
    TOKIO_WORKER_THREADS: '2', SYNTHETIC_CONTROL_KEY: controlKey, SYNTHETIC_WRAPPING_KEY: randomBytes(32).toString('base64') };
  process.once('SIGTERM', abort);
  process.once('SIGINT', abort);
  try {
    provider = fork(providerFile, [], { execArgv: [], env: { PATH: '/usr/bin:/bin', HOME: temp },
      stdio: ['ignore', 'ignore', 'ignore', 'ipc'] });
    children.add(provider);
    const startup = new AbortController();
    const quitting = () => startup.abort(Error('Synthetic peer stopped before readiness'));
    provider.once('exit', quitting);
    let port;
    try {
      [{ port }] = await once(provider, 'message', { signal: AbortSignal.any([startup.signal, AbortSignal.timeout(5000)]) });
    } finally {
      provider.off('exit', quitting);
    }
    const counters = async () => {
      const reply = once(provider, 'message', { signal: AbortSignal.timeout(5000) });
      provider.send('count');
      return (await reply)[0].counters;
    };
    const upstream = 'http://127.0.0.1:' + port;
    let origin = upstream;
    let tenancy;
    let startupMs = null;
    if (lane !== 'direct') {
      const inferencePort = await reserve();
      const controlPort = await reserve();
      try {
        const host = '127.0.0.1:' + inferencePort.address().port;
        const management = '127.0.0.1:' + controlPort.address().port;
        origin = 'http://' + host;
        const configuration = join(temp, 'fixture.yaml');
        const socket = join(temp, 'fixture.sock');
        const config = {
          inherit_defaults: false, registry: { enabled: false },
          server: { listen: host, control_socket: socket, skip_auth: lane !== 'tenant' },
          database: { url: 'sqlite://synthetic.db?mode=rwc' },
        };
        if (lane === 'tenant') {
          config.control = { enabled: true, listen: management, credentials: [
            { id: 'local-operator', token_env: 'SYNTHETIC_CONTROL_KEY', scopes: ['control:read', 'control:tenancy'] },
          ] };
          config.tenancy = { enabled: true, wrapping_key_env: 'SYNTHETIC_WRAPPING_KEY', resource_origins: [upstream] };
        } else {
          config.providers = { fixture: {
            api_base: upstream + '/v1', api_key: 'synthetic-loopback-key',
            api_protocol: [{ '*': 'chat_completions' }], models: ['short', 'paced', 'tools'].map(id => ({ id })),
          } };
        }
        await writeFile(configuration, JSON.stringify(config), { mode: 0o600 });
        await Promise.all([inferencePort, controlPort].map(server => new Promise(resolve => server.close(resolve))));
        let childFailure;
        const launched = performance.now();
        gateway = spawn(binary, ['--json', 'serve', '--config', configuration, '--managed-child'], {
          cwd: temp, env: runEnv, stdio: ['ignore', 'ignore', 'ignore'],
        });
        children.add(gateway);
        gateway.once('error', error => { childFailure = error; });
        await until(launched + 30_000, async () => {
          if (childFailure) throw childFailure;
          assert.equal(gateway.exitCode, null, 'gateway terminated before readiness');
          try {
            const owned = await status(socket);
            if (owned.pid !== gateway.pid || owned.listen !== host) return false;
            const health = await fetch(origin + '/health', { signal: AbortSignal.timeout(500), redirect: 'error' });
            await health.arrayBuffer();
            return health.ok;
          } catch { return false; }
        });
        startupMs = performance.now() - launched;
        if (lane === 'tenant') tenancy = await createTenant('http://' + management, controlKey, upstream);
      } finally {
        inferencePort.close();
        controlPort.close();
      }
    }
    driver = client(origin, protocol, tenancy?.headers ?? {}, lane === 'gateway' ? 'fixture:' : '');
    await delay(500);
    const pid = gateway?.pid ?? provider.pid;
    const idleKib = await memory(pid);
    processSamples.push(idleKib);
    const before = await counters();
    timer = setInterval(() => {
      if (!pendingSample) pendingSample = memory(pid).then(value => processSamples.push(value), error => {
        sampleError ??= error;
      }).finally(() => { pendingSample = undefined; });
    }, 250);
    const windows = {};
    let warmups = 0;
    for (const [scenario, planned] of Object.entries(counts)) {
      const warmed = await window(driver, ['long', 'cancel'].includes(scenario) ? 'json' : scenario,
        smoke ? 1 : CONCURRENCY, CONCURRENCY);
      warmups += warmed.rows.length;
      windows[scenario] = await window(driver, scenario, smoke ? 1 : planned, CONCURRENCY);
    }
    clearInterval(timer);
    await pendingSample;
    if (sampleError) throw sampleError;
    const rows = Object.values(windows).flatMap(value => value.rows);
    let after;
    await until(performance.now() + 8000, async () => {
      after = await counters();
      return after.active === 0;
    });
    const observed = Object.fromEntries(['started', 'completed', 'cancelled', 'failed'].map(key => [key, after[key] - before[key]]));
    const cancelled = rows.filter(row => row.cancel).length;
    assert.deepEqual(observed, {
      started: rows.length + warmups, completed: rows.length + warmups - cancelled, cancelled, failed: 0,
    });
    const receipts = tenancy ? await tenancy.check(rows) : 0;
    return { startup: startupMs, idleKib, peakKib: Math.max(...processSamples), processSamples,
      observed, warmups, rows: rows.length, receipts, windows,
    };
  } finally {
    clearInterval(timer);
    await pendingSample;
    driver?.close();
    await stop(gateway);
    await stop(provider);
    await rm(temp, { recursive: true, force: true });
    process.off('SIGTERM', abort);
    process.off('SIGINT', abort);
  }
}
