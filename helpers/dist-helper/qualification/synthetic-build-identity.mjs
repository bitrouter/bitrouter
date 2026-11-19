import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';

export const BASELINE = 'b0d7e2fbeabe39b96692c48787a066e89c04aed0';
export const TARGET = 'aarch64-apple-darwin';
export const VARIANTS = { minimum: [], http2: ['http2'] };
export const ROLES = {
  m0: { binary: 'bro', version: '1.0.0-beta.1' },
  service: { binary: 'bitrouter', version: '1.0.0-beta.2' },
};

export async function hash(path) {
  const digest = createHash('sha256');
  for await (const bytes of createReadStream(path)) digest.update(bytes);
  return digest.digest('hex');
}

export async function verifyBuild(receipt, executable, role, variant, source, lock, live = false) {
  const { binary, version } = ROLES[role];
  const identity = receipt.build_info;
  assert.deepEqual([receipt.sha, receipt.product, receipt.variant, receipt.binary, receipt.lock_sha256],
    [source, role, variant, binary, lock]);
  assert.deepEqual([identity.source_revision, identity.version, identity.target, identity.features],
    [source, version, TARGET, VARIANTS[variant]]);
  assert.equal(identity.profile, variant === 'minimum' ? 'gateway' : 'custom');
  if (role === 'service') assert.equal(identity.product, 'bitrouter');
  const settings = receipt.settings;
  assert.deepEqual(
    [settings.target, settings.jobs, settings.incremental, settings.rustflags, settings.variant,
      settings.strip, settings.profile, settings.cache, settings.dev_debug, settings.test_debug],
    [TARGET, 2, 0, '', variant, 'symbols', 'release', 'fresh target, prefetched locked registry', 0, 0],
  );
  assert.match(settings.rustc, /^rustc 1\.99\.1 \(/);
  assert.match(settings.rustc, /^host: aarch64-apple-darwin$/m);
  assert.match(settings.cargo, /^cargo 1\.99\.1 \(/);
  assert.match(settings.clang, /^Apple clang version /);
  assert.match(settings.sdk, /^\d+\.\d+(?:\.\d+)?$/);
  assert.equal(receipt.machine.arch, 'arm64');
  assert.match(receipt.binary_sha256, /^[0-9a-f]{64}$/);
  assert.equal(await hash(executable), receipt.binary_sha256);
  assert.equal((await stat(executable)).size, receipt.binary_bytes);
  const options = { encoding: 'utf8', timeout: 10_000, env: { PATH: '/usr/bin:/bin' } };
  execFileSync('/usr/bin/codesign', ['--verify', '--strict', executable], options);
  if (live) assert.deepEqual(JSON.parse(execFileSync(executable, ['build-info'], options)), identity);
}
