// Exercise the actual cargo-dist installers offline, with two tiny Cargo packages
// and two real versioned releases. Pass the pinned dist executable on PATH.
import assert from 'node:assert/strict';
import { execFile, execFileSync, spawn, spawnSync } from 'node:child_process';
import { appendFileSync, copyFileSync, cpSync, createReadStream, existsSync, linkSync, mkdirSync, readFileSync, renameSync, rmSync,
  symlinkSync, writeFileSync } from 'node:fs';
import { installEnv, tempRoot } from './environment.mjs';
import { dirname, join } from 'node:path';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { promisify } from 'node:util';

const scripts = fileURLToPath(new URL('../', import.meta.url));
const root = tempRoot('split-install-');
const windows = process.platform === 'win32';
const ext = windows ? '.exe' : '';
const names = { bitrouter: 'bitrouter', 'bitrouter-bro': 'bro' };
const versions = ['1.0.0-beta.1', '1.0.0-beta.2'];
const host = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)[1].trim();
const buildEnv = { ...process.env, CARGO_TARGET_DIR: join(root, 'target'), BITROUTER_BUILD_REVISION: 'fixture-revision' };
const dist = process.env.DIST || 'dist';
const run = (program, args) => execFileSync(program, args, { cwd: root, env: buildEnv, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
function write(path, data) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
}

function buildReleases() {
  run('git', ['init', '-q']);
  run('git', ['remote', 'add', 'origin', 'https://github.com/bitrouter/bitrouter.git']);
  // Keep the production four-target plan for validation and generate real
  // checksums for the one native target used by this offline test.
  const config = readFileSync(join(scripts, '../../../dist-workspace.toml'), 'utf8');
  write(join(root, 'dist-workspace.toml'), config.replace('[dist]', '[dist]\nsource-tarball = false'));
  write(join(root, 'Cargo.toml'), '[workspace]\nmembers = ["bitrouter","bitrouter-bro"]\nresolver = "3"\n[profile.dist]\ninherits = "release"\n');
  for (const version of versions) {
    for (const [pkg, binary] of Object.entries(names)) {
      write(join(root, pkg, 'Cargo.toml'), `[package]
name = "${pkg}"
version = "${version}"
edition = "2024"
repository = "https://github.com/bitrouter/bitrouter"
description = "packaging test fixture"
[[bin]]
name = "${binary}"
path = "src/main.rs"
`);
      const info = { version, target: host, source_revision: buildEnv.BITROUTER_BUILD_REVISION,
        product: pkg, ...(pkg === 'bitrouter' ? { profile: 'full' } : {}) };
      write(join(root, pkg, 'src/main.rs'), `fn main() {
  if std::env::args().nth(1).as_deref() == Some("--version") { println!("${binary} ${version}"); }
  else { println!("{}", r#"${JSON.stringify(info)}"#); }
}\n`);
    }
    const plan = join(root, 'plan.json');
    write(plan, run(dist, ['plan', '-o', 'json']));
    run(process.execPath, [join(scripts, 'release-plan.mjs'), plan]);
    for (const pkg of Object.keys(names)) {
      const scoped = join(root, 'scoped-plan.json');
      write(scoped, run(dist, ['plan', '--tag', `${pkg}-v${version}`, '-o', 'json']));
      const result = JSON.parse(run(process.execPath, [join(scripts, 'release-plan.mjs'), scoped]));
      assert.deepEqual(result.products.map(product => product.package), [pkg]);
    }
    const assets = join(root, 'target', 'distrib');
    write(join(assets, 'plan-dist-manifest.json'), readFileSync(plan));
    const args = ['--artifacts=local', '--target', host, '-o', 'json'];
    const local = run(dist, ['build', ...args]);
    const localManifest = join(assets, 'local-dist-manifest.json');
    write(localManifest, local);
    run(process.execPath, [join(scripts, 'verify-archives.mjs'), localManifest]);
    const global = join(root, 'global.json');
    write(global, run(dist, ['build', '--artifacts=global', '-o', 'json']));
    run(process.execPath, [join(scripts, 'guard-installers.mjs'), global]);
    cpSync(assets, join(root, version), { recursive: true });
    for (const pkg of Object.keys(names)) {
      rmSync(join(root, version, `${pkg}-${host}.${windows ? 'zip' : 'tar.xz'}.sha256`));
    }
    rmSync(assets, { recursive: true });
  }
}

function prefix(label) {
  const base = join(root, label);
  return { base, bin: join(base, 'cargo', 'bin'), config: join(base, 'config') };
}
const receipt = (p, pkg) => join(p.config, pkg, pkg + '-receipt.json');
const binary = (p, pkg) => join(p.bin, names[pkg] + ext);
function installer(p, pkg, version, interrupt, extraArgs = pkg === 'bitrouter-bro' && windows ? ['-BroOnly'] : [], overrides = {}) {
  const dir = join(root, version);
  let file = join(dir, pkg + '-installer.' + (windows ? 'ps1' : 'sh'));
  if (interrupt) {
    const script = readFileSync(file, 'utf8');
    const marker = interrupt === 'before' || interrupt === 'hold'
      ? (windows ? '  $dest_dir = New-Item -Force -ItemType Directory -Path $dest_dir' : '    say "installing to $_install_dir"')
      : (windows ? '  # Write the install receipt' : '    # Install the install receipt');
    assert.equal(script.split(marker).length, 2);
    file = join(dir, 'interrupt-' + pkg + (windows ? '.ps1' : '.sh'));
    let hook = windows ? '[Diagnostics.Process]::GetCurrentProcess().Kill()' : 'kill -KILL "$$"';
    if (interrupt === 'hold') {
      hook = windows ? `[IO.File]::WriteAllText((Join-Path $env:USERPROFILE 'ready'), '')
        while (!(Test-Path -LiteralPath (Join-Path $env:USERPROFILE 'resume'))) { Start-Sleep -Milliseconds 50 }`
        : 'touch "$HOME/ready"; while [ ! -f "$HOME/resume" ]; do sleep 0.1; done';
    }
    write(file, script.replace(marker, () => hook + '\n' + marker));
  }
  const env = { ...installEnv(p.base, join(p.base, 'cargo'), p.config),
    INSTALLER_DOWNLOAD_URL: pathToFileURL(dir).href, ...overrides };
  return windows ? ['powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', file, ...extraArgs], { env, encoding: 'utf8' }]
    : ['sh', [file, ...extraArgs], { env, encoding: 'utf8' }];
}
function install(p, pkg, version, refuse, interrupt, extraArgs, overrides) {
  const result = spawnSync(...installer(p, pkg, version, interrupt, extraArgs, overrides));
  const output = result.stdout + result.stderr;
  if (refuse) {
    assert.notEqual(result.status, 0);
    assert.match(output, refuse);
  } else {
    assert.equal(result.status, 0, output);
    const info = JSON.parse(execFileSync(binary(p, pkg)));
    assert.equal(info.version, version);
    const record = JSON.parse(readFileSync(receipt(p, pkg)));
    assert.equal(record.source.app_name, pkg);
    assert.deepEqual(record.binaries, [names[pkg] + ext]);
    assert.equal(record.binary_sha256, createHash('sha256').update(readFileSync(binary(p, pkg))).digest('hex'));
  }
}

function testInstalls() {
  const [old, next] = versions;
  for (const [first, second] of [Object.keys(names), Object.keys(names).reverse()]) {
    const p = prefix(first + '-first');
    install(p, first, old);
    assert.ok(!existsSync(binary(p, second)) && !existsSync(receipt(p, second)));
    install(p, second, old);
    for (const [owner, peer] of [[first, second], [second, first]]) {
      const peerRecord = readFileSync(receipt(p, peer));
      const peerBinary = readFileSync(binary(p, peer));
      for (const version of [next, old]) {
        install(p, owner, version);
        assert.deepEqual(readFileSync(receipt(p, peer)), peerRecord);
        assert.deepEqual(readFileSync(binary(p, peer)), peerBinary);
        assert.equal(JSON.parse(execFileSync(binary(p, peer))).version, old);
      }
    }
    writeFileSync(binary(p, first), 'manual replacement');
    install(p, first, next, /digest mismatch/);
    assert.equal(readFileSync(binary(p, first), 'utf8'), 'manual replacement');
  }
  const p = prefix('linked-products');
  for (const pkg of Object.keys(names)) install(p, pkg, old);
  const bro = binary(p, 'bitrouter-bro');
  const service = binary(p, 'bitrouter');
  const records = Object.fromEntries(Object.keys(names).map(pkg => [pkg, readFileSync(receipt(p, pkg))]));
  const original = readFileSync(service);
  rmSync(bro);
  if (windows) linkSync(service, bro); else symlinkSync(names.bitrouter, bro);
  for (const pkg of Object.keys(names)) {
    install(p, pkg, next, /[Rr]efusing.*linked/);
    assert.deepEqual(readFileSync(service), original);
    assert.deepEqual(readFileSync(bro), original);
    for (const peer of Object.keys(names)) assert.deepEqual(readFileSync(receipt(p, peer)), records[peer]);
  }

  // A manual archive (including an extracted reduced service) has no receipt.
  const manual = prefix('manual');
  write(binary(manual, 'bitrouter'), 'manual archive');
  install(manual, 'bitrouter', next, /[Rr]efusing|no matching receipt/);
  assert.equal(readFileSync(binary(manual, 'bitrouter'), 'utf8'), 'manual archive');

  // A copied receipt from a different prefix cannot authorize a replacement.
  const stale = prefix('stale-prefix');
  const source = prefix('bitrouter-bro-first');
  mkdirSync(stale.bin, { recursive: true });
  copyFileSync(binary(source, 'bitrouter'), binary(stale, 'bitrouter'));
  write(receipt(stale, 'bitrouter'), readFileSync(receipt(source, 'bitrouter')));
  install(stale, 'bitrouter', next, /digest mismatch/);

  for (const pkg of Object.keys(names)) {
    const recover = prefix('recovery-' + pkg);
    const lock = join(dirname(receipt(recover, pkg)), '.install-lock');
    const journal = join(lock, 'receipt');
    const binLock = join(recover.bin, '.' + pkg + '-install-lock');
    // Kill the generated installer itself, including its cleanup handler.
    install(recover, pkg, old, /.*/, 'after');
    assert.ok(!existsSync(receipt(recover, pkg)));
    assert.equal(JSON.parse(readFileSync(journal)).version, old);
    install(recover, pkg, old);
    install(recover, pkg, next, /.*/, 'before');
    assert.equal(JSON.parse(execFileSync(binary(recover, pkg))).version, old);
    if (windows) {
      // The native updater relocates the running EXE. Exercise its missing-image
      // window with the same retained recovery copy the updater would leave.
      renameSync(binary(recover, pkg), join(lock, 'previous.exe'));
    }
    install(recover, pkg, next);
    install(recover, pkg, old, /.*/, 'after');
    assert.equal(JSON.parse(readFileSync(receipt(recover, pkg))).version, next);
    const valid = readFileSync(journal);
    const original = readFileSync(binary(recover, pkg));
    writeFileSync(journal, JSON.stringify({ ...JSON.parse(valid), install_prefix: root }));
    install(recover, pkg, next, /prefix mismatch|prefix or binary digest mismatch/i);
    assert.deepEqual(readFileSync(binary(recover, pkg)), original);
    assert.equal(JSON.parse(readFileSync(journal)).install_prefix, root);
    writeFileSync(journal, valid);
    install(recover, pkg, old);
    assert.ok(!existsSync(lock) && !existsSync(binLock));
    mkdirSync(binLock); // Unmarked locks could belong to an active older writer.
    install(recover, pkg, next, /unrecognized installer lock/i);
    assert.ok(existsSync(binLock));
    assert.deepEqual(readFileSync(binary(recover, pkg)), original);
  }

  const archive = join(root, next, `bitrouter-${host}.${windows ? 'zip' : 'tar.xz'}`);
  const originalArchive = readFileSync(archive);
  try {
    appendFileSync(archive, 'damaged release');
    const receiptBefore = readFileSync(receipt(source, 'bitrouter'));
    install(source, 'bitrouter', next, /checksum mismatch/i);
    assert.deepEqual(readFileSync(receipt(source, 'bitrouter')), receiptBefore);
    assert.equal(JSON.parse(execFileSync(binary(source, 'bitrouter'))).version, old);
  } finally {
    writeFileSync(archive, originalArchive);
  }
}

async function testContention() {
  const p = prefix('held-lock'), other = { ...prefix('other-prefix'), config: p.config };
  const owner = 'bitrouter-bro';
  const holder = spawn(...installer(p, owner, versions[0], 'hold'));
  let diagnostic = '';
  holder.stdout.on('data', data => { diagnostic += data; });
  holder.stderr.on('data', data => { diagnostic += data; });
  const finished = new Promise((resolve, reject) => {
    holder.on('error', reject);
    holder.on('close', resolve);
  });
  try {
    const deadline = Date.now() + 60_000;
    while (!existsSync(join(p.base, 'ready'))) {
      assert.ok(holder.exitCode === null && Date.now() < deadline, 'installer did not take locks: ' + diagnostic);
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    for (const dest of [p, other]) {
      install(dest, owner, versions[1], /active installer/i);
      assert.ok(!existsSync(binary(dest, owner)));
    }
  } finally {
    write(join(p.base, 'resume'), '');
    assert.equal(await finished, 0, diagnostic);
  }
}

async function testCombinedInstallers() {
  const [version, next] = versions;
  const release = join(root, version);
  assert.match(readFileSync(join(release, 'bitrouter-bro.rb'), 'utf8'),
    /^  depends_on "bitrouter\/tap\/bitrouter"$/m);
  if (windows) {
    const p = prefix('windows-both');
    install(p, 'bitrouter-bro', version, undefined, undefined, []);
    const core = JSON.parse(execFileSync(binary(p, 'bitrouter')));
    assert.equal(core.product, 'bitrouter');
    assert.equal(core.version, version);
    const record = JSON.parse(readFileSync(receipt(p, 'bitrouter')));
    assert.deepEqual(record.binaries, ['bitrouter.exe']);
    assert.equal(record.source.app_name, 'bitrouter');
    assert.equal(record.binary_sha256,
      createHash('sha256').update(readFileSync(binary(p, 'bitrouter'))).digest('hex'));

    // A core initially installed into its own directory keeps its location
    // and independent receipt when a later BRO installer updates the pair.
    const custom = join(p.base, 'separate-core');
    const other = prefix('windows-custom-core');
    const override = { BITROUTER_INSTALL_DIR: custom };
    const customBin = join(custom, 'bin');
    install({ ...other, bin: customBin }, 'bitrouter', version, undefined, undefined, [], override);
    const archive = join(root, next, `bitrouter-${host}.zip`);
    const original = readFileSync(archive);
    const oldCore = readFileSync(join(customBin, 'bitrouter.exe'));
    const oldReceipt = readFileSync(receipt(other, 'bitrouter'));
    try {
      appendFileSync(archive, 'damaged core archive');
      install(other, 'bitrouter-bro', next,
        /BRO was installed.*Archive checksum mismatch/s, undefined, [], override);
      assert.equal(JSON.parse(execFileSync(binary(other, 'bitrouter-bro'))).version, next);
      assert.deepEqual(readFileSync(join(customBin, 'bitrouter.exe')), oldCore);
      assert.deepEqual(readFileSync(receipt(other, 'bitrouter')), oldReceipt);
    } finally {
      writeFileSync(archive, original);
    }
    install(other, 'bitrouter-bro', next, undefined, undefined, [], override);
    assert.equal(JSON.parse(execFileSync(join(customBin, 'bitrouter.exe'))).version, next);
    assert.ok(!existsSync(binary(other, 'bitrouter')));
  }

  const suffix = windows ? '.zip' : '.tar.xz';
  const archives = Object.keys(names).map(pkg => `${pkg}-${host}${suffix}`);
  const seen = [];
  let corruptCore = false;
  const server = createServer((req, res) => {
    const [, selected, name] = req.url.split('/');
    seen.push(name);
    if (!versions.includes(selected) || !archives.includes(name)) return res.writeHead(404).end();
    if (corruptCore && name === archives[0]) return res.end('damaged archive');
    createReadStream(join(root, selected, name)).pipe(res);
  });
  const app = join(root, 'npm-both');
  mkdirSync(app);
  const npm = windows
    ? [process.execPath, [join(dirname(process.execPath), 'node_modules/npm/bin/npm-cli.js')]]
    : ['npm', []];
  const env = { ...process.env, NO_PROXY: '127.0.0.1', no_proxy: '127.0.0.1' };
  const exec = promisify(execFile);
  try {
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const source = `http://127.0.0.1:${server.address().port}`;
    const binaries = { bro: 'run-bro.js', bitrouter: 'core/run-bitrouter.js' };
    function npmArchive(selected) {
      const directory = join(app, selected);
      mkdirSync(directory);
      execFileSync('tar', ['xf', join(root, selected, 'bitrouter-bro-npm-package.tar.gz'), '-C', directory]);
      for (const file of ['package/package.json', 'package/core/package.json']) {
        const path = join(directory, file);
        const pkg = JSON.parse(readFileSync(path));
        if (file === 'package/package.json') assert.deepEqual(pkg.bin, binaries);
        pkg.artifactDownloadUrls = [`${source}/${selected}`];
        write(path, JSON.stringify(pkg));
      }
      const archive = join(directory, 'bitrouter-bro.tgz');
      execFileSync('tar', ['czf', archive, '-C', directory, 'package']);
      return archive;
    }
    const archive = npmArchive(version);
    const cli = (prefix, ...args) => exec(npm[0],
      [...npm[1], '--global', '--prefix', prefix, ...args], { cwd: app, env, timeout: 180_000 });
    const installed = join(app, 'installed');
    const oldPackage = join(app, 'legacy');
    write(join(oldPackage, 'package.json'), JSON.stringify({
      name: 'bitrouter', version, bin: { bitrouter: 'run.js' },
    }));
    write(join(oldPackage, 'run.js'), '#!/usr/bin/env node\n');
    const [npmTool, npmArgs] = npm;
    const packed = await exec(npmTool, [...npmArgs, 'pack', '--json'], { cwd: oldPackage, env });
    const previous = join(oldPackage, JSON.parse(packed.stdout)[0].filename);
    await cli(installed, 'install', '--no-audit', '--no-fund', previous);
    const oldBin = windows ? join(installed, 'bitrouter.cmd') : join(installed, 'bin/bitrouter');
    const oldContents = readFileSync(oldBin);
    await assert.rejects(cli(installed, 'install', '--no-audit', '--no-fund', archive),
      error => /EEXIST/.test(error.stderr));
    assert.deepEqual(readFileSync(oldBin), oldContents);
    await cli(installed, 'uninstall', 'bitrouter');
    seen.length = 0;
    await cli(installed, 'install', '--no-audit', '--no-fund', archive);
    assert.deepEqual(seen.sort(), [...archives].sort());
    async function checkInstalled(selected) {
      for (const name of Object.keys(binaries)) {
        const bin = windows ? join(installed, name + '.cmd') : join(installed, 'bin', name);
        assert.ok(existsSync(bin), `missing npm command ${bin}`);
        const output = windows
          ? await exec('cmd.exe', ['/d', '/s', '/c', `""${bin}""`], { cwd: app, env, windowsVerbatimArguments: true })
          : await exec(bin, [], { cwd: app, env });
        const info = JSON.parse(output.stdout);
        assert.equal(info.product, name === 'bro' ? 'bitrouter-bro' : 'bitrouter');
        assert.equal(info.version, selected);
      }
    }
    await checkInstalled(version);
    seen.length = 0;
    await cli(installed, 'install', '--no-audit', '--no-fund', npmArchive(next));
    assert.deepEqual(seen.sort(), [...archives].sort());
    await checkInstalled(next);
    await cli(installed, 'uninstall', 'bitrouter-bro');
    for (const name of Object.keys(binaries)) {
      assert.ok(!existsSync(windows ? join(installed, name + '.cmd') : join(installed, 'bin', name)));
    }
    corruptCore = true;
    await assert.rejects(cli(join(app, 'corrupt'), 'install', '--no-audit', '--no-fund', archive),
      error => /Archive checksum mismatch/.test(error.stderr));
  } finally {
    server.close();
  }
}

try {
  buildReleases();
  await testContention();
  testInstalls();
  await testCombinedInstallers();
  console.log(`Passed release plans, packaged identity, install ownership, upgrade, rollback, process termination and lock contention on ${host}`);
} finally {
  rmSync(root, { recursive: true, force: true });
}
