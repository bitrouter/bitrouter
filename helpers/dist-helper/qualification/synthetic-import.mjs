import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { closeSync, openSync } from 'node:fs';
import { chmod, copyFile, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { BASELINE, ROLES, TARGET, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';

const REPO = 'bitrouter/bitrouter';
const BRANCH = 'codex/bitrouter-bro-m3-m5';
const WORKFLOW = '.github/workflows/split-qualification.yml';
const WORKFLOW_ID = 422573390;
const BASE_RUN = 44304019788;
const BASE_HARNESS = 'e9318e5f36c9de2457532caafc851fe80644ec89';
const BASE_ARTIFACTS = {
  minimum: [14427748470, 'sha256:fbe9fb4a2c873b9a439b00fd95a6fe624f1ca9cb610a77b7b413bbe3aeade91d'],
  http2: [14429036265, 'sha256:6cb993bef5bcc5b27ce54d40c164c091c8443516502ebe9d4503ce0fddcd083b'],
};
const variants = Object.keys(VARIANTS);
const waitMs = 35 * 60_000;
class Unavailable extends Error {}

function run(command, args, options = {}) {
  return execFileSync(command, args, {
    encoding: 'utf8', timeout: 45_000, stdio: ['ignore', 'pipe', 'pipe'], ...options,
  }).trim();
}

function api(path) {
  let body;
  try {
    body = run('gh', ['api', 'repos/' + REPO + '/' + path]);
  } catch {
    throw new Unavailable('Actions metadata unavailable');
  }
  return JSON.parse(body);
}

function trusted(info, head) {
  assert.equal(info.path, WORKFLOW);
  assert.equal(info.workflow_id, WORKFLOW_ID);
  assert.equal(info.head_sha, head);
  assert.equal(info.head_branch, BRANCH);
  assert.equal(info.repository.full_name, REPO);
  assert.equal(info.head_repository.full_name, REPO);
}

function deliveries(info, source) {
  const attempt = info.run_attempt;
  assert.ok(Number.isSafeInteger(attempt) && attempt >= 1);
  const path = 'actions/runs/' + info.id;
  const response = api(path + '/attempts/' + attempt + '/jobs?per_page=100');
  assert.ok(response.total_count <= 100);
  const jobs = variants.map(variant => response.jobs.filter(job => job.name === `runtime binary (${variant})`));
  if (jobs.some(group => group.length !== 1 || group[0].conclusion !== 'success')) return null;
  const artifacts = api(path + '/artifacts?per_page=100');
  assert.ok(artifacts.total_count <= 100);
  const choices = variants.map((variant, index) => {
    const name = `runtime-${source}-${variant}-${attempt}`;
    const found = artifacts.artifacts.filter(item => item.name === name && !item.expired);
    assert.ok(found.length <= 1);
    if (!found.length) return null;
    const [artifact] = found;
    const [job] = jobs[index];
    assert.match(artifact.digest, /^sha256:[0-9a-f]{64}$/);
    assert.equal(artifact.workflow_run.id, info.id);
    assert.equal(artifact.workflow_run.head_sha, info.head_sha);
    assert.equal(artifact.workflow_run.head_repository_id, info.head_repository.id);
    assert.equal(artifact.workflow_run.repository_id, info.repository.id);
    assert.equal(job.run_attempt, attempt);
    assert.ok(job.labels.includes('macos-15'));
    return { artifact, job, variant };
  });
  return choices.every(Boolean) ? choices : null;
}

async function exactDeliveries(candidate, runId, attempt, wait) {
  assert.match(runId, /^[1-9]\d*$/);
  assert.match(attempt, /^[1-9]\d*$/);
  const deadline = Date.now() + (wait ? waitMs : 0);
  let announced = false;
  do {
    const info = api('actions/runs/' + runId + '/attempts/' + attempt);
    trusted(info, candidate);
    assert.equal(String(info.id), runId);
    assert.equal(String(info.run_attempt), attempt);
    assert.ok(['pull_request', 'workflow_dispatch'].includes(info.event), 'Pinned producer was not a final qualification event');
    const available = deliveries(info, candidate);
    if (available) return { info, available };
    if (info.status === 'completed' || Date.now() >= deadline) break;
    if (!announced) console.log('Waiting for the two exact-source runtime binary jobs.');
    announced = true;
    await delay(Math.min(60_000, Math.max(0, deadline - Date.now())));
  } while (Date.now() < deadline);
  throw new Unavailable('No complete exact-source native artifact pair');
}

function download(id, destination) {
  const output = openSync(destination, 'wx', 0o600);
  try {
    execFileSync('gh', ['api', 'repos/' + REPO + '/actions/artifacts/' + id + '/zip'],
      { timeout: 90_000, stdio: ['ignore', output, 'pipe'] });
  } catch {
    throw new Unavailable('Actions archive download unavailable');
  } finally {
    closeSync(output);
  }
}

function member(command, args, output) {
  const file = openSync(output, 'wx', 0o600);
  try { execFileSync(command, args, { timeout: 45_000, stdio: ['ignore', file, 'pipe'] }); }
  finally { closeSync(file); }
}

export async function prepare(temp, role, source, lock, info, delivery) {
  const { artifact, job, variant } = delivery;
  const { binary } = ROLES[role];
  if (role === 'm0') assert.deepEqual([artifact.id, artifact.digest], BASE_ARTIFACTS[variant]);
  const directory = join(temp, `${role}-${variant}`);
  await mkdir(directory);
  const zip = join(directory, 'artifact.zip');
  const tar = join(directory, 'runtime.tar.gz');
  download(artifact.id, zip);
  assert.equal('sha256:' + await hash(zip), artifact.digest, 'Actions ZIP digest mismatch');
  const stem = role + '/' + variant + '/';
  const entries = run('/usr/bin/unzip', ['-Z1', zip]).split('\n');
  for (const name of ['runtime.tar.gz', 'build.json', 'cold-release.json']) {
    assert.equal(entries.filter(entry => entry === stem + name).length, 1);
  }
  const unpack = name => run('/usr/bin/unzip', ['-p', zip, stem + name]);
  const receipt = JSON.parse(unpack('build.json'));
  const cold = JSON.parse(unpack('cold-release.json'));
  const args = ['build', '--release', '--bin', binary, '--offline', '--locked', '-j', '2', '-p', 'bitrouter',
    '--target', TARGET, '--no-default-features'];
  if (variant === 'http2') args.push('--features', 'http2');
  args.push('--message-format=json-render-diagnostics');
  assert.deepEqual([cold.source, cold.profile, cold.status, cold.signal, cold.args], [source, 'release', 0, null, args]);
  assert.equal(String(receipt.run_id), String(info.id));
  assert.equal(String(receipt.run_attempt), String(info.run_attempt));
  assert.equal(receipt.workflow, REPO + '/' + WORKFLOW + '@' +
    (info.event === 'pull_request' ? 'refs/pull/1063/merge' : 'refs/heads/' + BRANCH));
  member('/usr/bin/unzip', ['-p', zip, stem + 'runtime.tar.gz'], tar);
  assert.deepEqual(run('/usr/bin/tar', ['-tzf', tar]).split('\n').sort(), [binary, 'build.json'].sort());
  assert.deepEqual(JSON.parse(run('/usr/bin/tar', ['-xOzf', tar, 'build.json'])), receipt);
  const executable = join(directory, binary);
  member('/usr/bin/tar', ['-xOzf', tar, binary], executable);
  await verifyBuild(receipt, executable, role, variant, source, lock);
  return { role, variant, binary, executable, receipt: { ...receipt, delivery: {
    run: info.id, attempt: info.run_attempt, job: job.id, artifact: artifact.id, digest: artifact.digest, head: info.head_sha,
  } } };
}

async function main(root, candidate, runId, attempt, wait) {
  assert.match(candidate, /^[0-9a-f]{40}$/);
  assert.notEqual(candidate, BASELINE);
  const base = api('actions/runs/' + BASE_RUN + '/attempts/1');
  trusted(base, BASE_HARNESS);
  assert.deepEqual([base.event, base.conclusion, base.run_attempt], ['push', 'success', 1]);
  const baseline = deliveries(base, BASELINE);
  if (!baseline) throw new Unavailable('Pinned M0 release artifacts are no longer available');
  const final = await exactDeliveries(candidate, runId, attempt, wait);
  if (wait) {
    console.log('Both pinned native producer jobs succeeded and their artifacts are available.');
    return;
  }
  const temp = await mkdtemp(join(root, 'synthetic-import-'));
  try {
    const builds = [];
    for (const [role, checkout, source, info, available] of [
      ['m0', 'baseline', BASELINE, base, baseline], ['service', 'candidate', candidate, final.info, final.available],
    ]) {
      assert.equal(run('git', ['-C', checkout, 'rev-parse', 'HEAD']), source);
      const lock = await hash(join(checkout, 'Cargo.lock'));
      for (const entry of available) builds.push(await prepare(temp, role, source, lock, info, entry));
    }
    for (const variant of variants) {
      const [left, right] = builds.filter(build => build.variant === variant);
      try {
        assert.deepEqual(left.receipt.settings, right.receipt.settings);
      } catch {
        throw new Unavailable(`Native producer settings do not match for ${variant}`);
      }
    }
    const provenance = join(root, 'synthetic-builds');
    await mkdir(provenance);
    for (const { role, variant, binary, executable, receipt } of builds) {
      const destination = join(root, 'executables', role, variant);
      await mkdir(destination, { recursive: true });
      await copyFile(executable, join(destination, binary));
      await chmod(join(destination, binary), 0o500);
      await writeFile(join(provenance, `${role}-${variant}.json`), JSON.stringify(receipt, null, 2) + '\n', { flag: 'wx' });
    }
    console.log(`Verified four exact-source runtime artifacts (candidate run ${final.info.id}, attempt ${final.info.run_attempt}).`);
  } finally {
    await rm(temp, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [root, candidate, runId, attempt, mode] = process.argv.slice(2);
    assert.ok(!mode || mode === '--wait');
    await main(resolve(root), candidate, runId, attempt, mode === '--wait');
  } catch (error) {
    if (error instanceof Unavailable) {
      console.log(error.message + '; rebuild both sides on the runner.');
      process.exitCode = 75;
    } else {
      console.error('Runtime artifact verification failed:', error.message);
      process.exitCode = 1;
    }
  }
}
