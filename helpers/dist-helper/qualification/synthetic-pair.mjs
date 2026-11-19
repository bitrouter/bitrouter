import assert from 'node:assert/strict';
import { execFileSync, fork } from 'node:child_process';
import { once } from 'node:events';
import { chmod, mkdir, readFile, writeFile } from 'node:fs/promises';
import { cpus, loadavg, release, totalmem } from 'node:os';
import { join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { trial, LANES } from './synthetic-trials.mjs';
import { summarize, compare } from './synthetic-score.mjs';
import { CONCURRENCY, COUNTS, LINUX_COUNTS, measuredLegs } from './synthetic-client.mjs';
import { BASELINE, LINUX_TARGET, ROLES, TARGET, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';
import { aggregateCpu, cpuInterval, linuxReadiness, snapshot } from './synthetic-linux-cpu.mjs';

const sourcePath = fileURLToPath(import.meta.url);
const linux = process.platform === 'linux';
const counts = Object.freeze(linux ? LINUX_COUNTS : COUNTS);
const HERE = fileURLToPath(new URL('.', import.meta.url));
const SOURCES = ['synthetic-peer.mjs', 'synthetic-client.mjs', 'synthetic-tenant.mjs',
  'synthetic-trials.mjs', 'synthetic-score.mjs', 'synthetic-pair.mjs', 'synthetic-build-identity.mjs',
  'synthetic-import.mjs', 'synthetic-linux-cpu.mjs', 'synthetic-linux-inputs.mjs', 'build.mjs'];
const save = (path, value) => writeFile(path, JSON.stringify(value, null, 2) + '\n', { flag: 'wx' });
const read = async path => JSON.parse(await readFile(path, 'utf8'));
const run = (program, args, cwd) => execFileSync(program, args, { cwd, encoding: 'utf8', timeout: 10_000 }).trim();
async function receipts(root, candidate) {
  const variants = {};
  for (const variant of Object.keys(VARIANTS)) {
    variants[variant] = {};
    for (const [role, source, checkout] of [['m0', BASELINE, 'baseline'], ['service', candidate, 'candidate']]) {
      const receipt = await read(join(root, 'synthetic-builds', `${role}-${variant}.json`));
      const binary = join(root, 'executables', role, variant, ROLES[role].binary);
      assert.equal(run('git', ['rev-parse', 'HEAD'], checkout), source);
      await verifyBuild(receipt, binary, role, variant, source, await hash(join(checkout, 'Cargo.lock')), true,
        linux ? LINUX_TARGET : TARGET);
      variants[variant][role] = { binary, receipt };
    }
    const { m0, service } = variants[variant];
    assert.deepEqual(m0.receipt.settings, service.receipt.settings);
    assert.equal(Boolean(m0.receipt.delivery), Boolean(service.receipt.delivery), 'mixed producer modes');
    if (!m0.receipt.delivery) assert.deepEqual(m0.receipt.machine, service.receipt.machine);
  }
  return variants;
}

async function settle(path) {
  if (linux) {
    const result = await linuxReadiness(performance.now() + 15 * 60_000);
    await save(path, result);
    console.log('Linux runner preflight:', JSON.stringify(result));
    assert.ok(result.passed, 'runner did not settle before measurement');
    return;
  }
  const observations = [];
  let streak = 0;
  const deadline = performance.now() + 15 * 60_000;
  while (performance.now() < deadline) {
    const top = run('/usr/bin/top', ['-l', '2', '-n', '0', '-s', '1']);
    const matches = [...top.matchAll(/(\d+(?:\.\d+)?)% idle/g)];
    const idle = Number(matches.at(-1)?.[1]);
    assert.ok(Number.isFinite(idle));
    observations.push({ utc: new Date().toISOString(), idle, load: loadavg() });
    streak = idle > 80 ? streak + 1 : 0;
    if (streak === 3) break;
    await delay(10_000);
  }
  await save(path, observations);
  const highestStreak = observations.reduce(({ current, highest }, { idle }) => {
    const next = idle > 80 ? current + 1 : 0;
    return { current: next, highest: Math.max(highest, next) };
  }, { current: 0, highest: 0 }).highest;
  const percentages = observations.map(({ idle }) => idle).sort((left, right) => left - right);
  console.log('Runner preflight:', JSON.stringify({
    side: path.endsWith('/baseline/idle.json') ? 'baseline' : 'candidate',
    samples: observations.length, above80: observations.filter(({ idle }) => idle > 80).length,
    idlePercent: { min: percentages[0], median: percentages[Math.floor(percentages.length / 2)], max: percentages.at(-1) },
    highestStreak, passed: streak === 3,
  }));
  assert.equal(streak, 3, 'runner did not settle before measurement');
}

async function freshTrial(binary, lane, protocol) {
  const child = fork(sourcePath, ['--trial', binary, lane, protocol], {
    env: { PATH: '/usr/bin:/bin' }, execArgv: [], stdio: ['ignore', 'ignore', 'ignore', 'ipc'],
  });
  let escalation;
  const stop = () => {
    child.kill('SIGTERM');
    escalation ??= setTimeout(() => child.kill('SIGKILL'), 5000);
  };
  const timeout = setTimeout(stop, 180_000);
  process.once('SIGTERM', stop);
  process.once('SIGINT', stop);
  let aggregate;
  child.once('message', value => { aggregate = value; });
  try {
    const [code] = await once(child, 'close');
    assert.equal(code, 0, 'isolated synthetic trial failed');
    assert.ok(aggregate);
    return aggregate;
  } finally {
    process.off('SIGTERM', stop);
    process.off('SIGINT', stop);
    clearTimeout(timeout);
    clearTimeout(escalation);
  }
}

async function collect(output, variants, role) {
  const records = [];
  await mkdir(output);
  await settle(join(output, 'idle.json'));
  for (const [group, lanes] of Object.entries(LANES)) {
    const [variant, protocol] = group.split('-');
    const { binary, receipt } = variants[variant][role];
    for (let repeat = 1; repeat <= 5; repeat++) {
      const offset = (repeat - 1) % lanes.length;
      const order = [...lanes.slice(offset), ...lanes.slice(0, offset)];
      for (const lane of order) {
        const file = join(output, [group, repeat, lane].join('-') + '.json');
        const started = new Date().toISOString();
        const record = { group, repeat, lane, started, loadBefore: loadavg() };
        try {
          if (linux) {
            const before = await snapshot();
            Object.assign(record, await freshTrial(binary, lane, protocol));
            record.cpu = cpuInterval(before, await snapshot());
          } else Object.assign(record, await trial(binary, lane, protocol, false));
        } catch (error) {
          await save(file, { ...record, failure: String(error.message), finished: new Date().toISOString() });
          throw error;
        }
        Object.assign(record, { loadAfter: loadavg(), finished: new Date().toISOString() });
        await save(file, record);
        records.push({ file, record, sha256: await hash(file) });
        console.log(role, group, repeat, lane, 'synthetic correctness passed');
      }
    }
    assert.equal(await hash(binary), receipt.binary_sha256);
  }
  return records;
}

export async function main(root, candidate, output) {
  assert.match(candidate, /^[0-9a-f]{40}$/);
  assert.notEqual(candidate, BASELINE);
  assert.ok(linux || process.platform === 'darwin');
  assert.equal(process.arch, linux ? 'x64' : 'arm64');
  assert.equal(process.version, 'v24.10.0');
  await mkdir(output);
  const inputs = async () => Object.fromEntries(await Promise.all(SOURCES.map(async file => [file, await hash(join(HERE, file))])));
  const built = await receipts(root, candidate);
  const method = { name: linux ? 'synthetic-gateway-split-linux-longer-fast-windows-v1' : 'synthetic-gateway-split-v1',
    baseline: BASELINE, candidate,
    producer: linux ? 'parallel-linux-release-builds' :
      built.minimum.m0.receipt.delivery ? 'verified-split-qualification' : 'same-run-release-builds',
    platform: process.platform, driver: linux ? 'fresh-per-trial' : 'shared',
    harness: process.env.GITHUB_SHA, host: { cpus: cpus().length, cpu: cpus()[0].model,
      memory: totalmem(), kernel: release(), node: process.version, tokio_workers: 2 },
    inputs: await inputs(), started: new Date().toISOString(),
  };
  if (linux) {
    method.readiness = { source: '/proc/stat', idleStrictlyAbovePercent: 80, consecutive: 3,
      sampleMs: 1000, spacingMs: 10_000, waitMinutes: 15, steal: 'recorded, not credited as idle' };
    method.order = '40 M0 trials, aggregate budget freeze, 40 candidate trials; existing lane rotation';
    method.workload = { conversationsPerTrial: counts, concurrency: CONCURRENCY, repeatsPerPath: 5,
      measuredLegsPerTrial: measuredLegs(counts), warmupLegsPerTrial: 24 };
  }
  const methodPath = join(output, 'method.json');
  await save(methodPath, method);
  const base = await collect(join(output, 'baseline'), built, 'm0');
  const budgetPath = join(output, 'baseline-budget.json');
  const baselineMetrics = summarize(base.map(entry => entry.record), counts);
  await save(budgetPath, { method: await hash(methodPath), frozen: new Date().toISOString(),
    budgets: baselineMetrics, trials: Object.fromEntries(base.map(({ file, sha256 }) => [file.split('/').at(-1), sha256])),
  });
  const frozen = await hash(budgetPath);
  await chmod(budgetPath, 0o444);
  await writeFile(join(output, 'baseline-budget.sha256'), frozen + '\n', { flag: 'wx' });
  assert.deepEqual(await inputs(), method.inputs);
  console.log('All baseline cohorts audited and budgets frozen.');
  const measured = await collect(join(output, 'candidate'), built, 'service');
  assert.equal(await hash(budgetPath), frozen);
  assert.deepEqual(await inputs(), method.inputs);
  for (const entry of base) assert.equal(await hash(entry.file), entry.sha256);
  const checked = await read(budgetPath);
  assert.equal(checked.method, await hash(methodPath));
  assert.deepEqual(checked.budgets, baselineMetrics);
  assert.ok(measured.every(entry => entry.record.started >= checked.frozen));
  const comparison = compare(checked.budgets, summarize(measured.map(entry => entry.record), counts));
  const unresolved = comparison.filter(row => row.controlDrift || row.status !== 'within-budget');
  await save(join(output, 'comparison.json'), { comparison, unresolved: unresolved.map(row => row.metric),
    ...(linux ? { controlDriftRows: comparison.filter(row => row.controlDrift).map(row => row.metric) } : {}),
  });
  await save(join(output, 'summary.json'), { baseline: BASELINE, candidate, budgetSha256: frozen,
    trialsPerSide: 40, measuredLegsPerSide: 40 * measuredLegs(counts), unresolved: unresolved.length,
    ...(linux ? { runtimeCpu: { baseline: aggregateCpu(base.map(entry => entry.record.cpu)),
      candidate: aggregateCpu(measured.map(entry => entry.record.cpu)) } } : {}),
  });
  assert.equal(unresolved.length, 0, 'synthetic runtime metrics require review');
}

if (process.argv[1] && resolve(process.argv[1]) === sourcePath) {
  const [first, second, third, fourth] = process.argv.slice(2);
  if (first === '--trial') {
    try { await new Promise((resolve, reject) => {
        trial(second, third, fourth, false, counts).then(
        value => process.send(value, error => error ? reject(error) : resolve()), reject);
    }); }
    catch { process.exitCode = 1; }
    finally { process.disconnect(); }
  } else {
    assert.ok(first && second && third);
    await main(resolve(first), second, resolve(third));
  }
}
