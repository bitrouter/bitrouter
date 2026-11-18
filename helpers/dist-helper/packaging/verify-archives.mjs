// Run on the native builder against the files listed by this local dist build.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';

const manifest = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const uploads = new Map(manifest.upload_files.map(path => [basename(path), path]));
const revision = process.env.BITROUTER_BUILD_REVISION;
assert.ok(revision);
for (const release of manifest.releases) {
  const pkg = release.app_name;
  const bin = pkg === 'bitrouter' ? 'bitrouter' : 'bro';
  const archives = release.artifacts.map(id => manifest.artifacts[id]).filter(a => a.kind === 'executable-zip' && uploads.has(a.name));
  assert.ok(archives.length > 0, `missing ${pkg} executable`);
  for (const asset of archives) {
    const path = uploads.get(asset.name);
    const [target] = asset.target_triples;
    const name = bin + (target.includes('windows') ? '.exe' : '');
    const digest = createHash('sha256').update(readFileSync(path)).digest('hex');
    assert.equal(digest, asset.checksums.sha256);
    const sidecar = readFileSync(join(dirname(path), asset.checksum), 'utf8').trim();
    assert.deepEqual(sidecar.split(/\s+\*?/), [digest, basename(path)]);
    const dir = mkdtempSync(join(tmpdir(), 'release-smoke-'));
    try {
      execFileSync('tar', ['xf', path, '-C', dir]);
      // cargo-dist uses a top directory for tarballs and a flat ZIP on Windows.
      const root = target.includes('windows') ? dir : join(dir, basename(path, '.tar.xz'));
      assert.ok(readdirSync(root).includes(name));
      const peer = bin === 'bro' ? 'bitrouter' : 'bro';
      assert.ok(!readdirSync(root).some(file => [peer, peer + '.exe'].includes(file)));
      const binary = join(root, name);
      assert.ok(statSync(binary).isFile());
      const info = JSON.parse(execFileSync(binary, ['--json', 'build-info'], { cwd: root }));
      assert.equal(info.version, release.app_version);
      assert.equal(info.target, target);
      assert.equal(info.source_revision, revision);
      assert.equal(info.product, pkg);
      if (pkg === 'bitrouter') assert.equal(info.profile, 'full');
      const version = execFileSync(binary, ['--version'], { cwd: root, encoding: 'utf8' }).trim();
      assert.equal(version.split(/\s+/).at(-1), release.app_version);
      execFileSync(binary, ['--help'], { cwd: root });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }
}
