import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createServer } from "node:net";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createRequire } from "node:module";

export const CALL_ID = "call_behavior_1";
export const TOOL_MARKER = "BITROUTER_BEHAVIOR_TOOL_OK";
export const FINAL_TEXT = "BITROUTER_BEHAVIOR_OK";

export type Protocol = "responses" | "messages";
export type Client = "pi" | "ai-sdk";

interface MockReady {
  uri: string;
}

interface CapturedLine {
  line: string;
  json?: unknown;
}

class LineCollector {
  readonly lines: CapturedLine[] = [];
  private buffer = "";

  push(chunk: Buffer): CapturedLine[] {
    this.buffer += chunk.toString("utf8");
    const parsed: CapturedLine[] = [];
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) break;
      const line = this.buffer.slice(0, newline).replace(/\r$/, "");
      this.buffer = this.buffer.slice(newline + 1);
      if (!line) continue;
      const captured = parseLine(line);
      this.lines.push(captured);
      parsed.push(captured);
    }
    return parsed;
  }
}

function parseLine(line: string): CapturedLine {
  const space = line.indexOf(" ");
  const candidate = space >= 0 ? line.slice(space + 1) : "";
  try {
    return { line, json: JSON.parse(candidate) };
  } catch {
    return { line };
  }
}

function forwardOutput(child: ChildProcessWithoutNullStreams, name: string) {
  const collector = new LineCollector();
  child.stdout.on("data", (chunk: Buffer) => {
    for (const line of collector.push(chunk)) {
      process.stderr.write(`[${name}] ${line.line}\n`);
    }
  });
  child.stderr.on("data", (chunk: Buffer) => {
    process.stderr.write(`[${name}:err] ${chunk.toString("utf8")}`);
  });
  return collector;
}

function waitForLine(
  collector: LineCollector,
  prefix: string,
  child: ChildProcessWithoutNullStreams,
  timeoutMs: number,
): Promise<CapturedLine> {
  return new Promise((resolvePromise, reject) => {
    const deadline = setTimeout(() => {
      cleanup();
      reject(new Error(`timed out waiting for ${prefix}`));
    }, timeoutMs);
    let poll: NodeJS.Timeout | undefined;
    const exit = (code: number | null, signal: NodeJS.Signals | null) => {
      cleanup();
      reject(
        new Error(
          `${prefix} process exited before readiness (code=${code}, signal=${signal})`,
        ),
      );
    };
    const cleanup = () => {
      clearTimeout(deadline);
      if (poll) clearInterval(poll);
      child.off("exit", exit);
    };
    const check = () => {
      const found = collector.lines.find((line) => line.line.startsWith(prefix));
      if (!found) return;
      cleanup();
      resolvePromise(found);
    };
    child.on("exit", exit);
    poll = setInterval(check, 25);
    check();
  });
}

function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function nodeBinary(): string {
  return requiredEnvironment("BITROUTER_TEST_NODE");
}

function bitrouterBinary(): string {
  return resolve(
    process.env.BITROUTER_TEST_GATEWAY_BINARY ?? "target/debug/bitrouter",
  );
}

function upstreamBinary(): string {
  return resolve(
    process.env.BITROUTER_TEST_BEHAVIOR_UPSTREAM ??
      "target/debug/bitrouter-behavior-upstream",
  );
}

function piEntrypoint(): string {
  const require = createRequire(import.meta.url);
  const packagePath = require.resolve(
    "@earendil-works/pi-coding-agent/package.json",
  );
  const packageJson = JSON.parse(
    readFileSync(packagePath, "utf8"),
  ) as { bin?: { pi?: string } };
  const entrypoint = packageJson.bin?.pi;
  if (!entrypoint) throw new Error("Pi package has no pi executable");
  return resolve(dirname(packagePath), entrypoint);
}

async function freePort(): Promise<number> {
  return new Promise((resolvePromise, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("could not allocate a loopback port"));
        return;
      }
      const port = address.port;
      server.close((error) =>
        error ? reject(error) : resolvePromise(port),
      );
    });
  });
}

async function waitHealthy(endpoint: string, child: ChildProcessWithoutNullStreams) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`bitrouter exited before readiness (${child.exitCode})`);
    }
    try {
      const response = await fetch(`${endpoint}/health`);
      if (response.ok) return;
    } catch {
      // Retry until the listener accepts requests.
    }
    await delay(50);
  }
  throw new Error(`bitrouter did not become healthy at ${endpoint}`);
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

export class BehaviorRun {
  private constructor(
    readonly root: string,
    readonly endpoint: string,
    readonly agentDir: string,
    readonly workspace: string,
    readonly inbound: Protocol,
    readonly outbound: Protocol,
    readonly bitrouter: ChildProcessWithoutNullStreams,
    readonly upstream: ChildProcessWithoutNullStreams,
    readonly upstreamLines: LineCollector,
  ) {}

  static async start(
    inbound: Protocol,
    outbound: Protocol,
  ): Promise<BehaviorRun> {
    const root = await mkdtemp(join(tmpdir(), "bitrouter-behavior-"));
    const workspace = join(root, "workspace");
    const agentDir = join(root, "pi-agent");
    await mkdir(workspace, { recursive: true });
    await mkdir(agentDir, { recursive: true });
    await writeFile(join(workspace, "fixture.txt"), `${TOOL_MARKER}\n`);

    const upstream = spawn(
      upstreamBinary(),
      ["--protocol", outbound, "--model", "test-model"],
      {
        cwd: root,
        env: cleanEnvironment(root),
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    const upstreamLines = forwardOutput(upstream, "upstream");
    const readyLine = await waitForLine(upstreamLines, "READY ", upstream, 15_000);
    const upstreamReady = readyLine.json as MockReady;
    if (!upstreamReady?.uri) throw new Error("mock upstream omitted its URI");

    const port = await freePort();
    const endpoint = `http://127.0.0.1:${port}`;
    const configPath = join(root, "bitrouter.yaml");
    const config = [
      "inherit_defaults: false",
      "registry:",
      "  enabled: false",
      "server:",
      `  listen: 127.0.0.1:${port}`,
      "  skip_auth: true",
      "  control_socket: control.sock",
      "database:",
      "  url: 'sqlite://bitrouter.db?mode=rwc'",
      "providers:",
      "  behavior:",
      `    api_base: '${upstreamReady.uri}'`,
      "    api_key: fixture",
      "    api_protocol:",
      `      - '*': ${outbound}`,
      "    models:",
      "      - id: test-model",
      "",
    ].join("\n");
    await writeFile(configPath, config);

    const bitrouter = spawn(
      bitrouterBinary(),
      ["serve", "--config", configPath],
      {
        cwd: root,
        env: cleanEnvironment(root),
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    forwardOutput(bitrouter, "bitrouter");
    try {
      await waitHealthy(endpoint, bitrouter);
    } catch (error) {
      bitrouter.kill("SIGKILL");
      upstream.kill("SIGKILL");
      await rm(root, { recursive: true, force: true });
      throw error;
    }

    await writeFile(
      join(agentDir, "models.json"),
      `${JSON.stringify(
        {
          providers: {
            "bitrouter-behavior": {
              baseUrl: inbound === "responses" ? `${endpoint}/v1` : endpoint,
              api:
                inbound === "responses"
                  ? "openai-responses"
                  : "anthropic-messages",
              apiKey: "local-placeholder",
              models: [{ id: "test-model", name: "Behavior fixture" }],
            },
          },
        },
        null,
        2,
      )}\n`,
    );

    return new BehaviorRun(
      root,
      endpoint,
      agentDir,
      workspace,
      inbound,
      outbound,
      bitrouter,
      upstream,
      upstreamLines,
    );
  }

  requests(): unknown[] {
    return this.upstreamLines.lines
      .filter((line) => line.line.startsWith("REQUEST "))
      .map((line) => line.json);
  }

  upstreamErrors(): string[] {
    return this.upstreamLines.lines
      .filter((line) => line.line.startsWith("ERROR "))
      .map((line) => line.line);
  }

  async runPi(): Promise<{
    settled: Record<string, unknown>;
    assistant: Record<string, unknown>;
    tools: Array<Record<string, unknown>>;
    exitCode: number | null;
    stderr: string;
  }> {
    const entrypoint = piEntrypoint();
    const child = spawn(
      nodeBinary(),
      [
        entrypoint,
        "--mode",
        "json",
        "--no-session",
        "--provider",
        "bitrouter-behavior",
        "--model",
        "test-model",
        "--tools",
        "read",
        "--no-approve",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-mcp",
        "--offline",
        "Read fixture.txt and then finish.",
      ],
      {
        cwd: this.workspace,
        env: {
          ...cleanEnvironment(this.root),
          PI_CODING_AGENT_DIR: this.agentDir,
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    const events: Array<Record<string, unknown>> = [];
    let buffer = "";
    let stderr = "";
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.stdout.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      for (;;) {
        const newline = buffer.indexOf("\n");
        if (newline < 0) break;
        const line = buffer.slice(0, newline).replace(/\r$/, "");
        buffer = buffer.slice(newline + 1);
        if (!line) continue;
        try {
          events.push(JSON.parse(line) as Record<string, unknown>);
        } catch (error) {
          stderr += `invalid Pi JSON line: ${String(error)}\n${line}\n`;
        }
      }
    });
    const exitCode = await waitChild(child, 90_000);
    if (buffer.trim()) {
      try {
        events.push(JSON.parse(buffer) as Record<string, unknown>);
      } catch {
        // Incomplete trailing data is diagnosed by the event assertions.
      }
    }
    const settled = [...events]
      .reverse()
      .find((event) => event.type === "agent_settled");
    const assistant = [...events]
      .reverse()
      .find((event) => {
        const message = event.message as Record<string, unknown> | undefined;
        return event.type === "message_end" && message?.role === "assistant";
      });
    const tools = events.filter((event) =>
      ["tool_execution_start", "tool_execution_end"].includes(
        String(event.type),
      ),
    );
    if (!settled) throw new Error(`Pi did not settle.\nstderr:\n${stderr}`);
    if (!assistant) throw new Error(`Pi did not emit an assistant message.\nstderr:\n${stderr}`);
    return {
      settled,
      assistant: assistant.message as Record<string, unknown>,
      tools,
      exitCode,
      stderr,
    };
  }

  async runAiSdk(): Promise<{
    text: string;
    finishReason: string;
    steps: Array<{
      toolCalls: Array<{ toolName: string; input: unknown }>;
      toolResults: Array<{ toolName: string; output: unknown }>;
    }>;
  }> {
    const { streamText, tool, jsonSchema, isStepCount } = await import("ai");
    const { createOpenAI } = await import("@ai-sdk/openai");
    const { createAnthropic } = await import("@ai-sdk/anthropic");
    const model =
      this.inbound === "responses"
        ? createOpenAI({
            baseURL: `${this.endpoint}/v1`,
            apiKey: "local-placeholder",
          }).responses("test-model")
        : createAnthropic({
            baseURL: `${this.endpoint}/v1`,
            apiKey: "local-placeholder",
          }).messages("test-model");
    const result = streamText({
      model,
      maxRetries: 0,
      stopWhen: isStepCount(2),
      tools: {
        read_fixture: tool({
          description: "Return the behavior fixture marker.",
          inputSchema: jsonSchema<Record<string, never>>({
            type: "object",
            properties: {},
            additionalProperties: false,
          }),
          execute: async () => ({ marker: TOOL_MARKER }),
        }),
      },
      prompt: "Read the fixture and then finish.",
    });
    for await (const part of result.fullStream) {
      if (part.type === "error") throw part.error;
    }
    return {
      text: await result.text,
      finishReason: await result.finishReason,
      steps: (await result.steps) as never,
    };
  }

  async dispose(): Promise<void> {
    await Promise.all([stopChild(this.bitrouter), stopChild(this.upstream)]);
    await rm(this.root, { recursive: true, force: true });
  }
}

function cleanEnvironment(home: string): NodeJS.ProcessEnv {
  const allowed = [
    "PATH",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
  ];
  const environment: NodeJS.ProcessEnv = {
    HOME: home,
    TMPDIR: tmpdir(),
  };
  for (const name of allowed) {
    const value = process.env[name];
    if (value) environment[name] = value;
  }
  return environment;
}

const LIVE_MODEL = "deepseek-flash";
const LIVE_RESULT = "BITROUTER_LIVE_OK";

export class LiveBehaviorRun {
  private constructor(
    readonly root: string,
    readonly endpoint: string,
    readonly agentDir: string,
    readonly workspace: string,
    readonly inbound: Protocol,
    readonly bitrouter: ChildProcessWithoutNullStreams,
  ) {}

  static async start(inbound: Protocol): Promise<LiveBehaviorRun> {
    const apiKey = requiredEnvironment("DEEPSEEK_BEHAVIOR_API_KEY");
    const root = await mkdtemp(join(tmpdir(), "bitrouter-behavior-live-"));
    const workspace = join(root, "workspace");
    const agentDir = join(root, "pi-agent");
    await mkdir(workspace, { recursive: true });
    await mkdir(agentDir, { recursive: true });

    const port = await freePort();
    const endpoint = `http://127.0.0.1:${port}`;
    const baseUrl =
      inbound === "responses"
        ? "https://api.deepseek.com"
        : "https://api.deepseek.com/anthropic/v1";
    const configPath = join(root, "bitrouter.yaml");
    await writeFile(
      configPath,
      [
        "inherit_defaults: false",
        "registry:",
        "  enabled: false",
        "server:",
        `  listen: 127.0.0.1:${port}`,
        "  skip_auth: true",
        "  control_socket: control.sock",
        "database:",
        "  url: 'sqlite://bitrouter.db?mode=rwc'",
        "providers:",
        "  deepseek:",
        `    api_base: ${JSON.stringify(baseUrl)}`,
        "    api_key: ${DEEPSEEK_BEHAVIOR_API_KEY}",
        "    api_protocol:",
        `      - '*': ${inbound}`,
        "    models:",
        `      - id: ${LIVE_MODEL}`,
        "",
      ].join("\n"),
    );
    const bitrouter = spawn(
      bitrouterBinary(),
      ["serve", "--config", configPath],
      {
        cwd: root,
        env: { ...cleanEnvironment(root), DEEPSEEK_BEHAVIOR_API_KEY: apiKey },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    forwardOutput(bitrouter, "bitrouter-live");
    try {
      await waitHealthy(endpoint, bitrouter);
      await writeFile(
        join(agentDir, "models.json"),
        `${JSON.stringify(
          {
            providers: {
              "bitrouter-live": {
                baseUrl: inbound === "responses" ? `${endpoint}/v1` : endpoint,
                api:
                  inbound === "responses"
                    ? "openai-responses"
                    : "anthropic-messages",
                apiKey: "local-placeholder",
                models: [{ id: LIVE_MODEL, name: "DeepSeek Flash" }],
              },
            },
          },
          null,
          2,
        )}\n`,
      );
    } catch (error) {
      bitrouter.kill("SIGKILL");
      await rm(root, { recursive: true, force: true });
      throw error;
    }
    return new LiveBehaviorRun(
      root,
      endpoint,
      agentDir,
      workspace,
      inbound,
      bitrouter,
    );
  }

  async runPi(): Promise<{
    settled: Record<string, unknown>;
    assistant: Record<string, unknown>;
    tools: Array<Record<string, unknown>>;
    stderr: string;
  }> {
    const child = spawn(
      nodeBinary(),
      [
        piEntrypoint(),
        "--mode",
        "json",
        "--no-session",
        "--provider",
        "bitrouter-live",
        "--model",
        LIVE_MODEL,
        "--tools",
        "write",
        "--no-approve",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-mcp",
        "--offline",
        `Use the write tool to create live-result.txt containing exactly ${LIVE_RESULT}. Do not use any other tool. Then finish.`,
      ],
      {
        cwd: this.workspace,
        env: {
          ...cleanEnvironment(this.root),
          PI_CODING_AGENT_DIR: this.agentDir,
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    const events: Array<Record<string, unknown>> = [];
    let buffer = "";
    let stderr = "";
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.stdout.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      for (;;) {
        const newline = buffer.indexOf("\n");
        if (newline < 0) break;
        const line = buffer.slice(0, newline).replace(/\r$/, "");
        buffer = buffer.slice(newline + 1);
        if (!line) continue;
        try {
          events.push(JSON.parse(line) as Record<string, unknown>);
        } catch {
          // A malformed event fails the assertions below.
        }
      }
    });
    const exitCode = await waitChild(child, 240_000);
    if (exitCode !== 0) {
      throw new Error(`Pi live run exited with code ${exitCode}: ${stderr}`);
    }
    const settled = [...events]
      .reverse()
      .find((event) => event.type === "agent_settled");
    const assistant = [...events]
      .reverse()
      .find((event) => {
        const message = event.message as Record<string, unknown> | undefined;
        return event.type === "message_end" && message?.role === "assistant";
      });
    if (!settled || !assistant) {
      throw new Error(`Pi live run did not settle: ${stderr}`);
    }
    return {
      settled,
      assistant: assistant.message as Record<string, unknown>,
      tools: events.filter((event) =>
        ["tool_execution_start", "tool_execution_end"].includes(String(event.type)),
      ),
      stderr,
    };
  }

  async runAiSdk(): Promise<{ text: string; finishReason: string }> {
    const { streamText, tool, jsonSchema, isStepCount } = await import("ai");
    const { createOpenAI } = await import("@ai-sdk/openai");
    const { createAnthropic } = await import("@ai-sdk/anthropic");
    const model =
      this.inbound === "responses"
        ? createOpenAI({
            baseURL: `${this.endpoint}/v1`,
            apiKey: "local-placeholder",
          }).responses(LIVE_MODEL)
        : createAnthropic({
            baseURL: `${this.endpoint}/v1`,
            apiKey: "local-placeholder",
          }).messages(LIVE_MODEL);
    const result = streamText({
      model,
      maxRetries: 0,
      maxOutputTokens: 4_096,
      // Use stateless tool history without provider-owned reasoning items.
      providerOptions:
        this.inbound === "responses"
          ? { openai: { store: false, forceReasoning: true, reasoningEffort: "none" } }
          : undefined,
      stopWhen: isStepCount(4),
      tools: {
        write_result: tool({
          description: "Write the requested result file.",
          inputSchema: jsonSchema<{ content: string }>({
            type: "object",
            properties: { content: { type: "string" } },
            required: ["content"],
            additionalProperties: false,
          }),
          execute: async ({ content }) => {
            await writeFile(join(this.workspace, "live-result.txt"), content);
            return { written: true };
          },
        }),
      },
      prompt: `Call write_result exactly once with content ${LIVE_RESULT}, then finish.`,
    });
    for await (const part of result.fullStream) {
      if (part.type === "error") throw part.error;
    }
    return {
      text: await result.text,
      finishReason: await result.finishReason,
    };
  }

  result(): string {
    return readFileSync(join(this.workspace, "live-result.txt"), "utf8");
  }

  async dispose(): Promise<void> {
    await stopChild(this.bitrouter);
    await rm(this.root, { recursive: true, force: true });
  }
}

function waitChild(
  child: ChildProcessWithoutNullStreams,
  timeoutMs: number,
): Promise<number | null> {
  return new Promise((resolvePromise, reject) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`child exceeded ${timeoutMs}ms`));
    }, timeoutMs);
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("exit", (code) => {
      clearTimeout(timer);
      resolvePromise(code);
    });
  });
}

async function stopChild(child: ChildProcessWithoutNullStreams): Promise<void> {
  if (child.exitCode !== null) return;
  child.kill("SIGTERM");
  const exited = await Promise.race([
    new Promise<boolean>((resolvePromise) =>
      child.once("exit", () => resolvePromise(true)),
    ),
    delay(3_000).then(() => false),
  ]);
  if (!exited) {
    child.kill("SIGKILL");
    await Promise.race([
      new Promise<void>((resolvePromise) =>
        child.once("exit", () => resolvePromise()),
      ),
      delay(1_000),
    ]);
  }
}
