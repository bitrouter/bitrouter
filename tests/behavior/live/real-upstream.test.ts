import { expect, test } from "vitest";
import { LiveBehaviorRun, type Protocol } from "../src/harness";

const LIVE_RESULT = "BITROUTER_LIVE_OK";

const PROTOCOLS: Protocol[] = ["responses", "messages"];

for (const client of ["pi", "ai-sdk"] as const) {
  for (const protocol of PROTOCOLS) {
    test(`${client}: live DeepSeek ${protocol} writes the result`, async () => {
      const run = await LiveBehaviorRun.start(protocol);
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
