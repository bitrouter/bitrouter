import { expect, test } from "vitest";
import {
  LiveBehaviorRun,
  type LiveProvider,
  type Protocol,
} from "../src/harness";

const LIVE_RESULT = "BITROUTER_LIVE_OK";

const CASES: Array<{
  provider: LiveProvider;
  protocol: Protocol;
  keyEnv: "OPENAI_API_KEY" | "ANTHROPIC_API_KEY";
  modelEnv: "BITROUTER_LIVE_OPENAI_MODEL" | "BITROUTER_LIVE_ANTHROPIC_MODEL";
}> = [
  {
    provider: "openai",
    protocol: "responses",
    keyEnv: "OPENAI_API_KEY",
    modelEnv: "BITROUTER_LIVE_OPENAI_MODEL",
  },
  {
    provider: "anthropic",
    protocol: "messages",
    keyEnv: "ANTHROPIC_API_KEY",
    modelEnv: "BITROUTER_LIVE_ANTHROPIC_MODEL",
  },
];

for (const client of ["pi", "ai-sdk"] as const) {
  for (const entry of CASES) {
    test(`${client}: live ${entry.provider} ${entry.protocol} writes the result`, async () => {
      const apiKey = process.env[entry.keyEnv];
      const model = process.env[entry.modelEnv];
      if (!apiKey || !model) {
        throw new Error(
          `${entry.keyEnv} and ${entry.modelEnv} are required for live behavior tests`,
        );
      }
      const run = await LiveBehaviorRun.start(
        entry.provider,
        entry.protocol,
        model,
        apiKey,
      );
      try {
        if (client === "pi") {
          const result = await run.runPi();
          expect(result.settled.aborted).toBe(false);
          expect(result.assistant.stopReason).toBe("stop");
          expect(
            result.tools.some(
              (event) =>
                event.type === "tool_execution_end" &&
                event.toolName === "write" &&
                event.isError === false,
            ),
          ).toBe(true);
        } else {
          const result = await run.runAiSdk();
          expect(result.finishReason).toBe("stop");
        }
        expect(run.result().trim()).toBe(LIVE_RESULT);
      } finally {
        await run.dispose();
      }
    });
  }
}
