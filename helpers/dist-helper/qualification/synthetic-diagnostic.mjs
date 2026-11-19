import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { BASELINE, ROLES, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';
import { trial } from './synthetic-trials.mjs';

const SOURCE = fileURLToPath(import.meta.url);
const CANDIDATE = 'cd48db41a76d87ef92670cf59a662870e18e0026';
const ORDER = ['m0', 'service', 'service', 'm0'];
const cases = [
  { key: 'minimum-instant-http1', variant: 'minimum', protocol: 'http1', scenario: 'instant' },
  { key: 'http2-tools-http1', variant: 'http2', protocol: 'http1', scenario: 'tools' },
  { key: 'http2-tools-h2c', variant: 'http2', protocol: 'h2c', scenario: 'tools' },
];
const save = (path, data) => writeFile(path, JSON.stringify(data, null, 2) + '\n', { flag: 'wx' });
const read = async path => JSON.parse(await readFile(path, 'utf8'));
const git = (checkout, ...args) => execFileSync('git', ['-C', checkout, ...args],
  { encoding: 'utf8', timeout: 10_000 }).trim();

export function plan() {
  const blocks = [];
  const block = (item, lane, concurrency, roles) => {
    const direct = lane === 'direct';
    blocks.push({ ...item, lane, concurrency, roles,
      conversations: direct ? 4096 : item.scenario === 'tools' ? 768 : 1024,
      warmups: direct ? 256 : 96, receiptSamples: 8 });
  };
  const sentinels = cases.slice(0, 2);
  for (const item of sentinels) block(item, 'direct', 4, ['m0']);
  for (const item of cases) {
    block(item, 'tenant', 4, ORDER);
    block(item, 'gateway', 4, ORDER);
    block(item, 'tenant', 1, ORDER);
  }
  for (const item of [...cases].reverse()) block(item, 'tenant', 4, ORDER);
  for (const item of [...sentinels].reverse()) block(item, 'direct', 4, ['m0']);
  return blocks;
}

function quantiles(values) {
  assert.ok(values.length && values.every(value => Number.isFinite(value) && value >= 0));
  const ordered = [...values].sort((left, right) => left - right);
  return Object.fromEntries([50, 95, 99].map(percent => ['p' + percent,
    ordered[Math.ceil(ordered.length * percent / 100) - 1]]));
}

export async function measure(binary, block) {
  const { lane, protocol, scenario, conversations, concurrency, warmups, receiptSamples } = block;
  const result = await trial(binary, lane, protocol, false,
    { scenario, count: conversations, concurrency, warmups, receiptSamples });
  const { rows, elapsed } = result.windows[scenario];
  const expectedLegs = scenario === 'tools' ? ['call', 'reply'] : ['reply'];
  assert.equal(rows.length, conversations * expectedLegs.length);
  assert.equal(result.warmups, warmups * expectedLegs.length);
  assert.equal(result.receipts, lane === 'tenant' ? receiptSamples * expectedLegs.length : 0);
  assert.ok(Number.isFinite(elapsed) && elapsed > 0);
  const measurements = {};
  for (const leg of expectedLegs) {
    const requests = rows.filter(row => row.leg === leg);
    assert.equal(requests.length, conversations);
    assert.ok(requests.every(row => row.version === (protocol === 'h2c' ? '2.0' : '1.1') && !row.cancel));
    measurements[leg] = {
      firstOutputMs: quantiles(requests.map(row => row.first)),
      terminalMs: quantiles(requests.map(row => row.elapsed)),
    };
  }
  return { elapsedMs: elapsed, conversations, measuredRequests: rows.length,
    conversationsPerSecond: 1000 * conversations / elapsed, requestsPerSecond: 1000 * rows.length / elapsed,
    receipts: result.receipts, observed: result.observed, measurements };
}

async function fresh(binary, block) {
  const child = spawn(process.execPath, [SOURCE, '--trial', binary, JSON.stringify(block)], {
    env: { PATH: '/usr/bin:/bin' }, stdio: ['ignore', 'pipe', 'ignore'],
  });
  const deadline = setTimeout(() => child.kill('SIGTERM'), 120_000);
  const stop = () => child.kill('SIGTERM');
  process.once('SIGTERM', stop);
  process.once('SIGINT', stop);
  try {
    const done = once(child, 'close');
    const body = (async () => {
      let output = '';
      for await (const chunk of child.stdout) output += chunk;
      return output;
    })();
    const [[code], output] = await Promise.all([done, body]);
    assert.equal(code, 0, 'synthetic trial failed');
    return JSON.parse(output);
  } finally {
    clearTimeout(deadline);
    process.off('SIGTERM', stop);
    process.off('SIGINT', stop);
  }
}

async function quiet() {
  const deadline = performance.now() + 15 * 60_000;
  let streak = 0;
  let samples = 0;
  while (performance.now() < deadline) {
    const report = execFileSync('/usr/bin/top', ['-l', '2', '-n', '0', '-s', '1'],
      { encoding: 'utf8', timeout: 10_000 });
    const idle = Number([...report.matchAll(/(\d+(?:\.\d+)?)% idle/g)].at(-1)?.[1]);
    assert.ok(Number.isFinite(idle));
    samples++;
    streak = idle > 80 ? streak + 1 : 0;
    if (streak === 3) return samples;
    await delay(10_000);
  }
  throw Error('runner did not settle at the preregistered idle gate');
}

async function main(root, output) {
  assert.equal(process.platform, 'darwin');
  assert.equal(process.arch, 'arm64');
  assert.equal(process.version, 'v24.10.0');
  const builds = {};
  for (const variant of Object.keys(VARIANTS)) {
    for (const [role, source, checkout, run] of [
      ['m0', BASELINE, 'baseline', 44304019788], ['service', CANDIDATE, 'candidate', 44477503080],
    ]) {
      const receipt = await read(join(root, 'synthetic-builds', `${role}-${variant}.json`));
      const binary = join(root, 'executables', role, variant, ROLES[role].binary);
      assert.equal(git(checkout, 'rev-parse', 'HEAD'), source);
      assert.deepEqual([receipt.delivery.run, receipt.delivery.attempt], [run, 1]);
      await verifyBuild(receipt, binary, role, variant, source, await hash(join(checkout, 'Cargo.lock')), true);
      builds[role + '/' + variant] = { binary, receipt };
    }
    assert.deepEqual(builds['m0/' + variant].receipt.settings, builds['service/' + variant].receipt.settings);
  }
  await mkdir(output);
  const blocks = plan();
  const method = {
    name: 'split-short-path-diagnostic-v1', acceptance: false, baseline: BASELINE, candidate: CANDIDATE,
    harness: git('harness', 'rev-parse', 'HEAD'), node: process.version, tokioWorkers: 2, blocks,
    readiness: { strictlyAboveIdlePercent: 80, consecutive: 3, spacingSeconds: 10, waitMinutes: 15 },
    freshProcessesPerTrial: ['node driver', 'synthetic mock', 'gateway when applicable'],
    pacing: 'closed loop; one timed fixed-count window per trial; warmup and receipt reads excluded',
    receiptValidation: 'eight stratified samples per leg; verify provider provenance and reported dimensions without inventing unreported usage',
    quantiles: 'nearest rank within each trial and tool leg; no filtering or pooled significance claims',
    provenance: Object.fromEntries(Object.entries(builds).map(([key, { receipt }]) =>
      [key, { ...receipt.delivery, binarySha256: receipt.binary_sha256 }])),
  };
  const methodPath = join(output, 'method.json');
  await save(methodPath, method);
  const methodSha256 = await hash(methodPath);
  const readinessSamples = await quiet();
  let attempted = 0, passed = 0;
  try {
    for (const [index, block] of blocks.entries()) {
      const aggregate = [];
      for (const [position, role] of block.roles.entries()) {
        attempted++;
        const { binary } = builds[role + '/' + block.variant];
        aggregate.push({ role, position: position + 1, ...await fresh(binary, block) });
        passed++;
      }
      await save(join(output, 'block-' + String(index + 1).padStart(2, '0') + '.json'),
        { methodSha256, block: index + 1, aggregate });
      console.log('Finished synthetic block', index + 1, block.key, block.lane, 'C' + block.concurrency);
    }
  } finally {
    await save(join(output, 'summary.json'), { methodSha256, readinessSamples, attempted, passed,
      planned: blocks.reduce((count, block) => count + block.roles.length, 0), acceptance: false });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === SOURCE) {
  const [first, second, third] = process.argv.slice(2);
  if (first === '--trial') {
    try { console.log(JSON.stringify(await measure(second, JSON.parse(third)))); }
    catch { process.exitCode = 1; }
  } else await main(resolve(first), resolve(second));
}
