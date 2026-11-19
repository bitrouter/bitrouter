import assert from 'node:assert/strict';
import { execFileSync, fork } from 'node:child_process';
import { once } from 'node:events';
import { chmod, mkdir, readFile, writeFile } from 'node:fs/promises';
import { cpus, loadavg, release, totalmem } from 'node:os';
import { join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { trial, LANES } from './synthetic-trials.mjs';
import { auditTrial, summarize, compare } from './synthetic-score.mjs';
import { CONCURRENCY, COUNTS, LINUX_COUNTS, measuredLegs } from './synthetic-client.mjs';
import { BASELINE, LINUX_TARGET, ROLES, TARGET, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';
import { aggregateCpu, cpuInterval, linuxReadiness, snapshot } from './synthetic-linux-cpu.mjs';

const sourcePath = fileURLToPath(import.meta.url);
const linux = process.platform === 'linux';
const counts = Object.freeze(linux ? LINUX_COUNTS : COUNTS);
const focused = linux && process.env.SYNTHETIC_FOCUSED === '1';
const gatewayTools = { tools: LINUX_COUNTS.tools };
const directTools = { tools: 8192 };
const focusMatrix = {
  'minimum-http1': { direct: directTools, gateway: gatewayTools, tenant: gatewayTools },
  'http2-http1': { direct: directTools, gateway: gatewayTools, tenant: gatewayTools },
  'http2-h2c': { gateway: gatewayTools, tenant: gatewayTools },
};
const countsFor = (group, lane) => focused ? focusMatrix[group][lane] : counts;
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

async function freshTrial(binary, lane, protocol, planned) {
  const child = fork(sourcePath, ['--trial', binary, lane, protocol, JSON.stringify(planned)], {
    env: { PATH: '/usr/bin:/bin' }, execArgv: [], stdio: ['ignore', 'ignore', 'inherit', 'ipc'],
  });
  let escalation;
  const stop = () => {
    child.kill('SIGTERM');
    escalation ??= setTimeout(() => child.kill('SIGKILL'), 5000);
  };
  const timeout = setTimeout(stop, 180_000);
  process.once('SIGTERM', stop);
  process.once('SIGINT', stop);
  const messages = [];
  child.on('message', value => { messages.push(value); });
  try {
    const [code] = await once(child, 'close');
    assert.equal(code, 0, 'isolated synthetic trial failed');
    assert.equal(messages.length, 1, 'isolated trial did not send exactly one record');
    const [message] = messages;
    assert.equal(message?.kind, 'trial-result', 'isolated trial returned an invalid message');
    const { startup, idleKib, peakKib, processSamples, observed, warmups, rows, receipts, windows } = message.result;
    return { startup, idleKib, peakKib, processSamples, observed, warmups, rows, receipts, windows };
  } finally {
    process.off('SIGTERM', stop);
    process.off('SIGINT', stop);
    clearTimeout(timeout);
    clearTimeout(escalation);
  }
}

async function collectTrial(output, source, role, group, repeat, lane) {
  const protocol = group.split('-')[1];
  const { binary } = source;
  const planned = countsFor(group, lane);
  const file = join(output, [group, repeat, lane].join('-') + '.json');
  const started = new Date().toISOString();
  const record = { group, repeat, lane, started, loadBefore: loadavg() };
  try {
    if (linux) {
      const before = await snapshot();
      Object.assign(record, await freshTrial(binary, lane, protocol, planned));
      record.cpu = cpuInterval(before, await snapshot());
    } else Object.assign(record, await trial(binary, lane, protocol, false, planned));
    auditTrial(record, planned);
  } catch (error) {
    await save(file, { ...record, failure: String(error.message), finished: new Date().toISOString() });
    throw error;
  }
  Object.assign(record, { loadAfter: loadavg(), finished: new Date().toISOString() });
  await save(file, record);
  console.log(role, group, repeat, lane, 'synthetic correctness passed');
  return { file, record, sha256: await hash(file) };
}

async function collect(output, variants, role, sentinelOutput) {
  const records = [];
  const sentinels = [];
  await mkdir(output);
  if (sentinelOutput) await mkdir(sentinelOutput);
  await settle(join(output, 'idle.json'));
  for (const [group, lanes] of Object.entries(LANES)) {
    const variant = group.split('-')[0];
    for (let repeat = 1; repeat <= 5; repeat++) {
      const offset = (repeat - 1) % lanes.length;
      const order = [...lanes.slice(offset), ...lanes.slice(0, offset)];
      for (const lane of order) {
        const retestM0 = async () => {
          sentinels.push(await collectTrial(sentinelOutput, variants[variant].m0, 'sentinel', group, repeat, lane));
        };
        const baselineFirst = (repeat + lanes.indexOf(lane)) % 2 === 0;
        if (sentinelOutput && baselineFirst) await retestM0();
        records.push(await collectTrial(output, variants[variant][role], role, group, repeat, lane));
        if (sentinelOutput && !baselineFirst) await retestM0();
      }
    }
  }
  for (const variant of Object.values(variants)) {
    assert.equal(await hash(variant[role].binary), variant[role].receipt.binary_sha256);
    if (sentinelOutput) assert.equal(await hash(variant.m0.binary), variant.m0.receipt.binary_sha256);
  }
  return { records, sentinels };
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
  const method = { name: focused ? 'synthetic-gateway-split-linux-tools-v1' :
    linux ? 'synthetic-gateway-split-linux-longer-fast-windows-v1' : 'synthetic-gateway-split-v1',
    baseline: BASELINE, candidate,
    producer: linux ? 'parallel-linux-release-builds' :
      built.minimum.m0.receipt.delivery ? 'verified-split-qualification' : 'same-run-release-builds',
    platform: process.platform, driver: linux ? 'fresh-per-trial' : 'shared',
    harness: process.env.GITHUB_SHA, host: { cpus: cpus().length, cpu: cpus()[0].model,
      memory: totalmem(), kernel: release(), node: process.version, tokio_workers: 2 },
    inputs: await inputs(), started: new Date().toISOString(),
  };
  if (linux) {
    const { run, attempt } = built.minimum.m0.receipt.delivery;
    method.producerRun = { run, attempt };
    method.readiness = { source: '/proc/stat', idleStrictlyAbovePercent: 80, consecutive: 3,
      sampleMs: 1000, spacingMs: 10_000, waitMinutes: 15, steal: 'recorded, not credited as idle' };
    method.order = '40 M0 trials, aggregate budget freeze, 40 candidate trials; existing lane rotation';
    method.workload = { conversationsPerTrial: counts, concurrency: CONCURRENCY, repeatsPerPath: 5,
      measuredLegsPerTrial: measuredLegs(counts), warmupLegsPerTrial: 24 };
  }
  if (focused) {
    method.order = 'Five M0 repeats per path and freeze before candidate. Five candidate repeats with contemporaneous M0 sentinels, alternating order within each path.';
    method.workload = { concurrency: CONCURRENCY, repeatsPerPath: 5, matrix: focusMatrix };
    method.priorRun = '44534157776';
    method.scope = 'Tool conversations in all original profiles. Use longer direct windows to distinguish mock drift from gateway changes. Process measurements are exploratory for this tools-only fixture.';
    method.sentinelPurpose = 'Compare M0 at the start and beside each candidate trial to characterize timing drift. The initial frozen budget remains the decision reference.';
  }
  const summarizeRecords = samples => summarize(samples.map(entry => entry.record), counts, focused ? focusMatrix : undefined);
  const methodPath = join(output, 'method.json');
  await save(methodPath, method);
  const { records: base } = await collect(join(output, 'baseline'), built, 'm0');
  const budgetPath = join(output, 'baseline-budget.json');
  const baselineMetrics = summarizeRecords(base);
  await save(budgetPath, { method: await hash(methodPath), frozen: new Date().toISOString(),
    budgets: baselineMetrics, trials: Object.fromEntries(base.map(({ file, sha256 }) => [file.split('/').at(-1), sha256])),
  });
  const frozen = await hash(budgetPath);
  await chmod(budgetPath, 0o444);
  await writeFile(join(output, 'baseline-budget.sha256'), frozen + '\n', { flag: 'wx' });
  assert.deepEqual(await inputs(), method.inputs);
  console.log('All baseline cohorts audited and budgets frozen.');
  const { records: measured, sentinels } = await collect(join(output, 'candidate'), built, 'service',
    focused ? join(output, 'sentinels') : undefined);
  assert.equal(await hash(budgetPath), frozen);
  assert.deepEqual(await inputs(), method.inputs);
  for (const entry of base) assert.equal(await hash(entry.file), entry.sha256);
  const checked = await read(budgetPath);
  assert.equal(checked.method, await hash(methodPath));
  assert.deepEqual(checked.budgets, baselineMetrics);
  assert.ok(measured.every(entry => entry.record.started >= checked.frozen));
  const current = summarizeRecords(measured);
  const comparison = compare(checked.budgets, current);
  if (focused) {
    const contemporary = summarizeRecords(sentinels);
    await save(join(output, 'sentinel-comparison.json'), {
      m0Repeat: compare(checked.budgets, contemporary),
      concurrentComparison: compare(contemporary, current),
      sentinels: sentinels.map(({ file, record, sha256 }) => ({ name: file.split('/').at(-1), started: record.started, sha256 })),
    });
  }
  const scored = focused ? comparison.filter(row => row.metric.split('/')[2] === 'tools') : comparison;
  const unresolved = scored.filter(row => row.controlDrift || row.status !== 'within-budget');
  await save(join(output, 'comparison.json'), { comparison, unresolved: unresolved.map(row => row.metric),
    ...(linux ? { controlDriftRows: comparison.filter(row => row.controlDrift).map(row => row.metric) } : {}),
  });
  await save(join(output, 'summary.json'), { baseline: BASELINE, candidate, budgetSha256: frozen,
    trialsPerSide: 40, measuredLegsPerSide: measured.reduce((sum, entry) => sum + entry.record.rows, 0), unresolved: unresolved.length,
    ...(linux ? { runtimeCpu: { baseline: aggregateCpu(base.map(entry => entry.record.cpu)),
      candidate: aggregateCpu(measured.map(entry => entry.record.cpu)) } } : {}),
  });
  assert.equal(unresolved.length, 0, 'synthetic runtime metrics require review');
}

if (process.argv[1] && resolve(process.argv[1]) === sourcePath) {
  const [first, second, third, fourth, fifth] = process.argv.slice(2);
  if (first === '--trial') {
    try { await new Promise((resolve, reject) => {
        trial(second, third, fourth, false, JSON.parse(fifth)).then(
        result => process.send({ kind: 'trial-result', result }, error => error ? reject(error) : resolve()), reject);
    }); }
    catch (error) {
      console.error('Synthetic trial failed:', error.name, error.message.split('\n')[0]);
      process.exitCode = 1;
    }
    finally { process.disconnect(); }
  } else {
    assert.ok(first && second && third);
    await main(resolve(first), second, resolve(third));
  }
}
