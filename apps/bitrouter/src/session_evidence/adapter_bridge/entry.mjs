// A dedicated Node entry keeps loader hooks out of NODE_OPTIONS/execArgv.
// https://nodejs.org/download/release/v22.17.0/docs/api/module.html#moduleregisterhooksoptions
import * as modules from "node:module";
import { readFileSync, realpathSync, existsSync } from "node:fs";
import { dirname, join, delimiter } from "node:path";
import { pathToFileURL } from "node:url";
import { adapters, transform } from "./transform.mjs";

const [harness, separator, ...args] = process.argv.slice(2);
const adapter = adapters[harness];
if (!adapter || separator !== "--") throw new Error("invalid native bridge launch");

function declaration(directory) {
  const manifestPath = join(directory, "package.json");
  if (!existsSync(manifestPath)) return null;
  const text = readFileSync(manifestPath, "utf8");
  if (Buffer.byteLength(text) > 1024 * 1024) throw new Error("adapter manifest size limit");
  const manifest = JSON.parse(text);
  if (manifest.name !== adapter.package) return null;
  const bin = typeof manifest.bin === "string" ? manifest.bin : manifest.bin?.[adapter.bin];
  if (typeof bin !== "string") throw new Error("adapter executable declaration missing");
  return { directory, manifest, entry: realpathSync(join(directory, bin)) };
}

function locate() {
  for (const directory of (process.env.PATH ?? "").split(delimiter)) {
    for (const name of [adapter.bin, `${adapter.bin}.cmd`, `${adapter.bin}.exe`]) {
      const candidate = join(directory, name);
      if (!existsSync(candidate)) continue;
      let parent = dirname(realpathSync(candidate));
      for (let depth = 0; depth < 8; depth++) {
        const found = declaration(parent);
        if (found) return found;
        const next = dirname(parent);
        if (next === parent) break;
        parent = next;
      }
      for (const packageDirectory of [join(dirname(directory), adapter.package), join(directory, "node_modules", adapter.package)]) {
        const found = declaration(packageDirectory);
        if (found) return found;
      }
      throw new Error("cannot resolve the selected native adapter");
    }
  }
  throw new Error("native adapter executable missing from package PATH");
}

const selected = locate();
const target = pathToFileURL(join(selected.directory, adapter.module)).href;
let hooks;
if (selected.manifest.version === adapter.version && typeof modules.registerHooks === "function") {
  hooks = modules.registerHooks({
    load(url, context, nextLoad) {
      const result = nextLoad(url, context);
      if (url !== target || result.format !== "module" || result.source == null) return result;
      // Hash the bytes actually supplied by the loader chain, not a separate
      // disk read. Unknown bytes are executed unchanged and cannot emit proof.
      const source = transform(harness, result.source, new URL("./runtime.mjs", import.meta.url).href);
      return source === null ? result : { ...result, source };
    },
  });
}
process.argv = [process.execPath, selected.entry, ...args];
try {
  await import(pathToFileURL(selected.entry).href);
} finally {
  hooks?.deregister();
}
