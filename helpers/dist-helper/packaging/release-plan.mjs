// Keep the pre-build inventory separate from the published digest manifest.
import assert from 'node:assert/strict';
import { readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { verifiedArchiveDigest } from './verify-archives.mjs';

const targets = ['aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu',
  'x86_64-pc-windows-msvc'];
const [mode, planPath, assetDir] = process.argv.slice(2);
const plan = JSON.parse(readFileSync(mode === '--finalize' ? planPath : mode, 'utf8'));
const inventory = project(plan);
const result = mode === '--finalize' ? await finalize(inventory, assetDir) : inventory;
console.log(JSON.stringify(result, null, 2));

function project(plan) {
  const tag = plan.announcement_tag;
  const scoped = tag.match(/(?:^|\/)(bitrouter(?:-bro)?)[-/]v?\d/);
  const expected = scoped ? [scoped[1]] : ['bitrouter', 'bitrouter-bro'];
  assert.deepEqual(plan.releases.map(r => r.app_name).sort(), expected);
  const products = plan.releases.map(({ app_name: pkg, app_version: version, artifacts }) => {
    const bin = pkg === 'bitrouter' ? 'bitrouter' : 'bro';
    const installers = ['-installer.sh', '-installer.ps1', '.rb', '-npm-package.tar.gz'].map(suffix => pkg + suffix);
    for (const name of installers) assert.ok(artifacts.includes(name), `missing ${name}`);
    const distributions = targets.flatMap(target => {
      const windows = target.includes('windows');
      const binary = bin + (windows ? '.exe' : '');
      const archive = `${pkg}-${target}.${windows ? 'zip' : 'tar.xz'}`;
      const checksum = archive + '.sha256';
      assert.ok(artifacts.includes(archive) && artifacts.includes(checksum), `missing ${archive} or checksum`);
      const declared = plan.artifacts[archive];
      assert.equal(declared.checksum, checksum);
      assert.deepEqual(declared.target_triples, [target]);
      assert.deepEqual(declared.assets.filter(a => a.kind === 'executable').map(a => a.path), [binary]);
      const primary = { target, binary, archive, checksum };
      const variants = [primary];
      if (pkg === 'bitrouter') {
        primary.profile = 'full';
        for (const profile of ['gateway', 'gateway-shared']) {
          const archive = `bitrouter-${profile}-${target}.${windows ? 'zip' : 'tar.gz'}`;
          variants.push({ profile, target, binary, archive, checksum: archive + '.sha256' });
        }
      }
      return variants;
    });
    assert.deepEqual(
      artifacts.filter(id => plan.artifacts[id].kind === 'executable-zip').sort(),
      distributions.filter(d => !d.profile || d.profile === 'full').map(d => d.archive).sort(),
      `unexpected native archives for ${pkg}`,
    );
    return { package: pkg, version, executable: bin, installers, distributions };
  });
  return { tag, source_revision: process.env.BITROUTER_BUILD_REVISION ?? null, products };
}

async function finalize({ tag, source_revision, products }, dir) {
  assert.ok(tag?.trim() && source_revision?.trim(), 'missing release tag or source revision');
  const finalized = [];
  for (const { distributions, ...product } of products) {
    for (const name of product.installers) {
      const file = statSync(join(dir, name));
      assert.ok(file.isFile() && file.size > 0, `empty or invalid installer ${name}`);
    }
    const archives = [];
    for (const { archive, checksum, ...distribution } of distributions) {
      assert.equal(checksum, archive + '.sha256');
      const sha256 = await verifiedArchiveDigest(join(dir, archive));
      archives.push({ ...distribution, archive, sha256 });
    }
    finalized.push({ ...product, distributions: archives });
  }
  return { schema: 'bitrouter.distribution.v1', tag, source_revision, products: finalized };
}
