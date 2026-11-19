import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { chmod, mkdir, readFile, writeFile } from 'node:fs/promises';
import { cpus, loadavg, release, totalmem } from 'node:os';
import { join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { trial, LANES } from './synthetic-trials.mjs';
import { summarize, compare } from './synthetic-score.mjs';
import { BASELINE, ROLES, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const SOURCES = ['synthetic-peer.mjs', 'synthetic-client.mjs', 'synthetic-tenant.mjs',
  'synthetic-trials.mjs', 'synthetic-score.mjs', 'synthetic-pair.mjs', 'synthetic-build-identity.mjs',
  'synthetic-import.mjs', 'build.mjs'];
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
      await verifyBuild(receipt, binary, role, variant, source, await hash(join(checkout, 'Cargo.lock')), true);
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
  const observations = [];
  let streak = 0;
  for (let sample = 0; sample < 16; sample++) {
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
  assert.equal(streak, 3, 'runner did not settle before measurement');
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
          Object.assign(record, await trial(binary, lane, protocol, false));
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
  assert.equal(process.platform, 'darwin');
  assert.equal(process.arch, 'arm64');
  assert.equal(process.version, 'v24.10.0');
  await mkdir(output);
  const inputs = async () => Object.fromEntries(await Promise.all(SOURCES.map(async file => [file, await hash(join(HERE, file))])));
  const built = await receipts(root, candidate);
  const method = { name: 'synthetic-gateway-split-v1', baseline: BASELINE, candidate,
    producer: built.minimum.m0.receipt.delivery ? 'verified-split-qualification' : 'same-run-release-builds',
    harness: process.env.GITHUB_SHA, host: { cpus: cpus().length, cpu: cpus()[0].model,
      memory: totalmem(), kernel: release(), node: process.version, tokio_workers: 2 },
    inputs: await inputs(), started: new Date().toISOString(),
  };
  const methodPath = join(output, 'method.json');
  await save(methodPath, method);
  const base = await collect(join(output, 'baseline'), built, 'm0');
  const budgetPath = join(output, 'baseline-budget.json');
  const baselineMetrics = summarize(base.map(entry => entry.record));
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
  const comparison = compare(checked.budgets, summarize(measured.map(entry => entry.record)));
  const unresolved = comparison.filter(row => row.controlDrift || row.status !== 'within-budget');
  await save(join(output, 'comparison.json'), { comparison, unresolved: unresolved.map(row => row.metric) });
  await save(join(output, 'summary.json'), { baseline: BASELINE, candidate, budgetSha256: frozen,
    trialsPerSide: 40, measuredLegsPerSide: 23040, unresolved: unresolved.length });
  assert.equal(unresolved.length, 0, 'synthetic runtime metrics require review');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [root, candidate, output] = process.argv.slice(2);
  assert.ok(root && candidate && output);
  await main(resolve(root), candidate, resolve(output));
}
