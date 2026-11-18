// Run from a candidate commit. Build two native cargo-dist releases of the
// actual applications. Only the package versions differ in the disposable clone.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { closeSync, copyFileSync, mkdirSync, openSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tempRoot } from './environment.mjs';

const scripts = fileURLToPath(new URL('../', import.meta.url));
const repo = fileURLToPath(new URL('../../../../', import.meta.url));
const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: repo, encoding: 'utf8' }).trim();
const host = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)[1].trim();
const root = tempRoot('updater-ci-'), work = join(root, 'src');
const versions = ['1.0.0-beta.901', '1.0.0-beta.902'];
const env = { ...process.env, BITROUTER_BUILD_REVISION: revision, CARGO_TARGET_DIR: join(work, 'target'),
  CARGO_PROFILE_DIST_LTO: 'false', CARGO_PROFILE_DIST_OPT_LEVEL: '0', CARGO_PROFILE_DIST_DEBUG: '0', CARGO_INCREMENTAL: '0' };
const dist = process.env.DIST || 'dist';
const run = (program, args, stdout = 'inherit') => execFileSync(program, args, { cwd: work, env, stdio: ['ignore', stdout, 'inherit'] });
function save(program, args, path) {
  const file = openSync(path, 'w');
  try { run(program, args, file); } finally { closeSync(file); }
}

try {
  execFileSync('git', ['clone', '--quiet', '--local', '--no-hardlinks', repo, work], { stdio: 'inherit' });
  run('git', ['checkout', '--quiet', '--detach', revision]);
  run('git', ['remote', 'set-url', 'origin', 'https://github.com/bitrouter/bitrouter.git']);
  const cfg = join(work, 'dist-workspace.toml');
  writeFileSync(cfg, readFileSync(cfg, 'utf8').replace('[dist]', '[dist]\nsource-tarball = false'));
  const apps = ['bitrouter', 'bro'].map(app => join(work, 'apps', app, 'Cargo.toml'));
  const original = apps.map(path => readFileSync(path, 'utf8'));
  const outputs = [];
  for (const version of versions) {
    const tag = 'v' + version;
    console.log(`Building production archives for ${tag} on ${host} from ${revision}`);
    for (const [i, path] of apps.entries()) {
      assert.match(original[i], /^version\.workspace = true$/m);
      writeFileSync(path, original[i].replace(/^version\.workspace = true$/m, `version = "${version}"`));
    }
    // Cargo resolves the two local package version changes against the clone's lock.
    run('cargo', ['metadata', '--format-version', '1', '--filter-platform', host], 'ignore');
    const assets = join(work, 'target', 'distrib');
    mkdirSync(assets, { recursive: true });
    const plan = join(root, 'plan.json');
    save(dist, ['plan', '--tag', tag, '-o', 'json'], plan);
    copyFileSync(plan, join(assets, 'plan-dist-manifest.json'));
    save(process.execPath, [join(scripts, 'release-plan.mjs'), plan], join(assets, 'distribution-plan.json'));
    const local = join(root, 'local.json');
    save(dist, ['build', '--artifacts=local', '--tag', tag, '--target', host, '-o', 'json'], local);
    run(process.execPath, [join(scripts, 'verify-archives.mjs'), local]);
    copyFileSync(local, join(assets, 'local-dist-manifest.json'));
    const global = join(root, 'global.json');
    save(dist, ['build', '--artifacts=global', '--tag', tag, '-o', 'json'], global);
    run(process.execPath, [join(scripts, 'guard-installers.mjs'), global]);
    const dest = join(root, tag);
    renameSync(assets, dest);
    outputs.push(dest);
  }
  // update.mjs also creates tiny cargo-dist fixture projects. Do not share the
  // native build directory or override their source revision.
  execFileSync(process.execPath, [join(scripts, 'tests/update.mjs'), '--archives', ...outputs],
    { cwd: repo, stdio: 'inherit' });
} finally {
  rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
