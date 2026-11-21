// cargo-dist compares its working directory to Cargo's canonical workspace
// path. Windows TEMP can contain the short spelling of the runner's profile.
import { mkdtempSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export function tempRoot(prefix) {
  return realpathSync.native(mkdtempSync(join(process.env.RUNNER_TEMP || tmpdir(), prefix)));
}

// PS5 must load its system modules even when the runner uses PowerShell 7.
// Keep installer redirects, GitHub Actions path hooks and credentials outside
// the fixture. Use the native tar.exe for cargo-dist ZIPs on Windows.
export function installEnv(home, cargo, config) {
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) =>
    /^(PATH|PATHEXT|SYSTEMROOT|WINDIR|COMSPEC|TEMP|TMP|TMPDIR|LANG)$/i.test(key)));
  if (process.platform === 'win32') {
    const system = join(process.env.SystemRoot, 'System32');
    env.PSModulePath = join(system, 'WindowsPowerShell', 'v1.0', 'Modules');
    const pathKey = Object.keys(env).find(k => k.toLowerCase() === 'path');
    env[pathKey] = system + ';' + env[pathKey];
  }
  return { ...env, HOME: home, USERPROFILE: home, CARGO_HOME: cargo, XDG_CONFIG_HOME: config,
    LOCALAPPDATA: config, INSTALLER_NO_MODIFY_PATH: '1' };
}
