import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { BASELINE, LINUX_TARGET, ROLES, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';

const [folder, temp, candidate] = process.argv.slice(2).map((value, index) => index < 2 ? resolve(value) : value);
const read = async file => JSON.parse(await readFile(file, 'utf8'));
const run = (command, args) => execFileSync(command, args, { encoding: 'utf8', timeout: 10_000 }).trim();
const runId = process.env.GITHUB_RUN_ID;
const attempt = process.env.GITHUB_RUN_ATTEMPT;
const peers = {};
assert.match(candidate, /^[0-9a-f]{40}$/);
assert.match(runId, /^\d+$/);
assert.match(attempt, /^[1-9]\d*$/);
for (const [role, source, checkout] of [['m0', BASELINE, 'baseline'], ['service', candidate, 'candidate']]) {
  assert.equal(run('git', ['-C', checkout, 'rev-parse', 'HEAD']), source);
  peers[role] = {};
  for (const variant of Object.keys(VARIANTS)) {
    const name = 'synthetic-linux-input-' + attempt + '-' + role + '-' + variant;
    const directory = join(folder, name);
    const receipt = await read(join(directory, 'build.json'));
    const { binary } = ROLES[role];
    assert.equal(receipt.workflow, 'bitrouter/bitrouter/.github/workflows/split-synthetic-linux.yml@' + process.env.GITHUB_REF);
    assert.equal(receipt.run_id, runId);
    assert.equal(receipt.run_attempt, attempt);
    const tar = join(directory, 'runtime.tar.gz');
    const digest = await hash(tar);
    assert.equal((await readFile(join(directory, 'runtime.sha256'), 'utf8')).trim(), digest);
    assert.deepEqual(run('/usr/bin/tar', ['-tzf', tar]).split('\n').sort(), [binary, 'build.json'].sort());
    assert.deepEqual(JSON.parse(run('/usr/bin/tar', ['-xOzf', tar, 'build.json'])), receipt);
    const release = await read(join(directory, 'cold-release.json'));
    const args = ['build', '--release', '--bin', binary, '--offline', '--locked', '-j', '2', '-p', 'bitrouter',
      '--target', LINUX_TARGET, '--no-default-features'];
    if (variant === 'http2') args.push('--features', 'http2');
    args.push('--message-format=json-render-diagnostics');
    assert.deepEqual([release.source, release.profile, release.status, release.signal, release.args],
      [source, 'release', 0, null, args]);
    const destination = join(temp, 'executables', role, variant);
    await mkdir(destination, { recursive: true });
    const executable = join(destination, binary);
    await writeFile(executable, execFileSync('/usr/bin/tar', ['-xOzf', tar, binary],
      { timeout: 30_000, maxBuffer: 200 * 1024 * 1024 }), { flag: 'wx', mode: 0o500 });
    await verifyBuild(receipt, executable, role, variant, source, await hash(join(checkout, 'Cargo.lock')), true, LINUX_TARGET);
    peers[role][variant] = { ...receipt, delivery: { run: runId, attempt, artifactName: name, payloadSha256: digest } };
  }
}
for (const variant of Object.keys(VARIANTS)) assert.deepEqual(peers.m0[variant].settings, peers.service[variant].settings);
const builds = join(temp, 'synthetic-builds');
await mkdir(builds);
for (const [role, variants] of Object.entries(peers)) {
  for (const [variant, receipt] of Object.entries(variants)) {
    await writeFile(join(builds, `${role}-${variant}.json`), JSON.stringify(receipt, null, 2) + '\n', { flag: 'wx' });
  }
}
console.log('Verified exact Linux releases, same-attempt delivery, ELF architecture and matching producer settings.');
