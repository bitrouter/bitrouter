// Run from a disposable product checkout. All measurements live under RUNNER_TEMP.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, appendFileSync, closeSync, copyFileSync, lstatSync,
  mkdirSync, mkdtempSync, openSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { arch, cpus, hostname, release, totalmem } from 'node:os';
import { join } from 'node:path';

const { SOURCE_SHA: sha, ROLE: role, VARIANT: variant, RUNNER_TEMP: temp } = process.env;
const mode = process.argv[2];
assert.ok(['timing', 'runtime'].includes(mode));
assert.ok(['m0', 'service', 'bro'].includes(role) && /^[0-9a-f]{40}$/.test(sha));
const options = {
  minimum: ['--no-default-features'], default: [],
  shared: ['--no-default-features', '--features', 'multi-instance'],
  http2: ['--no-default-features', '--features', 'http2'],
}[variant];
assert.ok(options && (role !== 'bro' || variant === 'default'));
const pkg = role === 'bro' ? 'bitrouter-bro' : 'bitrouter';
const binary = role === 'service' ? 'bitrouter' : 'bro';
const out = join(temp, 'm5-evidence', role, variant);
mkdirSync(out, { recursive: true });
const env = { ...process.env, CARGO_INCREMENTAL: '0', CARGO_BUILD_JOBS: '2',
  CARGO_PROFILE_RELEASE_STRIP: 'symbols', CARGO_PROFILE_TEST_DEBUG: '0', CARGO_PROFILE_DEV_DEBUG: '0' };
for (const key of ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'BITROUTER_BUILD_REVISION']) delete env[key];
const run = (cmd, args) => execFileSync(cmd, args, { env, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }).trim();
const emit = (file, data) => writeFileSync(join(out, file), JSON.stringify(data, null, 2) + '\n', { flag: 'wx' });
async function digest(path) {
  const hash = createHash('sha256');
  for await (const buffer of createReadStream(path)) hash.update(buffer);
  return hash.digest('hex');
}
function targetBytes(path) {
  const inodes = new Set();
  function size(dir) {
    let bytes = 0;
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const file = join(dir, entry.name);
      if (entry.isDirectory()) bytes += size(file);
      if (!entry.isFile()) continue;
      const st = lstatSync(file, { bigint: true });
      const inode = st.dev + '/' + st.ino;
      if (!inodes.has(inode)) bytes += Number(st.size);
      inodes.add(inode);
    }
    return bytes;
  }
  return size(path);
}

assert.equal(run('git', ['rev-parse', 'HEAD']), sha);
assert.equal(run('git', ['status', '--porcelain']), '');
const rustc = run('rustc', ['-vV']);
const target = rustc.match(/^host: (.+)$/m)[1];
assert.equal(target, mode === 'runtime' ? 'aarch64-apple-darwin' : 'x86_64-unknown-linux-gnu');
const common = ['--offline', '--locked', '-j', '2', '-p', pkg, '--target', target, ...options];
const settings = { rustc, cargo: run('cargo', ['-V']), target, jobs: 2, incremental: 0,
  rustflags: '', variant, strip: 'symbols', profile: 'release',
  cache: 'fresh target, prefetched locked registry', dev_debug: 0, test_debug: 0 };
if (mode === 'runtime') {
  settings.clang = run('xcrun', ['clang', '--version']);
  settings.sdk = run('xcrun', ['--show-sdk-version']);
}
const provenance = { sha, product: role, variant, lock_sha256: await digest('Cargo.lock'),
  workflow: process.env.GITHUB_WORKFLOW_REF, run_id: process.env.GITHUB_RUN_ID,
  run_attempt: process.env.GITHUB_RUN_ATTEMPT, repeat: process.env.REPEAT,
  machine: { hostname: hostname(), arch: arch(), kernel: release(), logical_cpus: cpus().length,
    cpu: cpus()[0].model, memory_bytes: totalmem(), image: process.env.ImageVersion }, settings };
emit('environment.json', provenance);
run('cargo', ['fetch', '--locked', '--target', target]);
if (mode === 'timing') {
  const output = run('cargo', ['metadata', '--offline', '--locked', '--no-deps', '--format-version', '1']);
  writeFileSync(join(out, 'workspace.json'), output);
  for (const [label, edges] of [['production', 'normal,build'], ['tests', 'normal,build,dev,features']]) {
    const tree = run('cargo', ['tree', '--offline', '--locked', '--target', target, '-p', pkg,
      ...options, '-e', edges, '--prefix', 'none']);
    writeFileSync(join(out, label + '-tree.txt'), tree + '\n');
    if (role === 'service') assert.doesNotMatch(tree, /^bitrouter-bro v/m, 'BRO in service closure');
  }
}

function build(label, profile) {
  const args = profile === 'test' ? ['test', '--no-run'] : ['build', '--release', '--bin', binary];
  args.push(...common, '--message-format=json-render-diagnostics');
  const jsonl = join(out, label + '.jsonl');
  const stdout = openSync(jsonl, 'wx'), stderr = openSync(join(out, label + '.stderr'), 'wx');
  const start = performance.now();
  let proc;
  try { proc = spawnSync('cargo', args, { env, stdio: ['ignore', stdout, stderr] }); }
  finally { closeSync(stdout); closeSync(stderr); }
  const seconds = (performance.now() - start) / 1000;
  if (proc.error) throw proc.error;
  const artifacts = readFileSync(jsonl, 'utf8').split('\n').filter(Boolean).map(JSON.parse)
    .filter(r => r.reason === 'compiler-artifact');
  const nonfresh = artifacts.filter(r => !r.fresh).map(r => ({ package: r.package_id, target: r.target.name }));
  const tests = artifacts.filter(r => r.executable && r.profile.test).map(r => ({
    package: r.package_id, name: r.target.name, kinds: r.target.kind, bytes: statSync(r.executable).size }));
  const result = { source: run('git', ['rev-parse', 'HEAD']), profile, args, seconds, status: proc.status,
    signal: proc.signal, target_bytes: targetBytes(env.CARGO_TARGET_DIR), nonfresh, tests };
  emit(label + '.json', result);
  assert.equal(proc.status, 0, 'Cargo failed in ' + label + ' (see stderr artifact)');
  if (profile === 'test') assert.ok(tests.length, 'empty test build');
  return result;
}
function identity(revision) {
  const exe = join(env.CARGO_TARGET_DIR, target, 'release', binary);
  const info = JSON.parse(run(exe, ['--json', 'build-info']));
  assert.equal(info.source_revision, revision);
  assert.equal(info.target, target);
  if (role !== 'm0') assert.equal(info.product, pkg);
  const expected = { minimum: [], shared: ['db-postgres', 'multi-instance'], http2: ['http2'] }[variant];
  if (expected) assert.deepEqual([...info.features].sort(), expected);
  return { exe, info };
}
for (const profile of mode === 'runtime' ? ['release'] : ['release', 'test']) {
  const targetDir = mkdtempSync(join(temp, 'm5-target-'));
  env.CARGO_TARGET_DIR = targetDir;
  try {
    build('cold-' + profile, profile);
    if (profile !== 'release') continue;
    const { exe, info } = identity(sha);
    emit('build.json', { ...provenance, binary, binary_bytes: statSync(exe).size,
      binary_sha256: await digest(exe), build_info: info });
    if (mode === 'runtime') {
      run('codesign', ['--verify', '--strict', exe]);
      copyFileSync(exe, join(out, binary));
      run('tar', ['-czf', join(out, 'runtime.tar.gz'), '-C', out, binary, 'build.json']);
      rmSync(join(out, binary));
    }
    if (mode === 'timing' && role === 'service' && variant === 'minimum' && process.env.REPEAT === '1') {
      const file = 'apps/bro/src/main.rs';
      run('git', ['ls-files', '--error-unmatch', file]);
      assert.equal(build('warm-control', profile).nonfresh.length, 0);
      appendFileSync(file, '\n// M5 private-source invalidation probe.\n');
      assert.equal(build('private-edit-fixed-head', profile).nonfresh.length, 0);
      identity(sha);
      // This isolated checkout has no persisted credentials. Advance its real Git HEAD.
      run('git', ['-c', 'user.name=M5 fixture', '-c', 'user.email=m5@localhost',
        '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null', 'commit', '-m', 'local M5 probe', '--', file]);
      const probe = run('git', ['rev-parse', 'HEAD']);
      build('private-edit-new-head', profile);
      emit('probe-identity.json', identity(probe).info);
      // Restore the measured revision before the independently cold test build.
      run('git', ['checkout', '--detach', sha]);
      assert.equal(run('git', ['status', '--porcelain']), '');
    }
  } finally {
    rmSync(targetDir, { recursive: true, force: true });
  }
}
