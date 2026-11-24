import { expect, test } from "vitest";
import {
  BehaviorRun,
  CALL_ID,
  FINAL_TEXT,
  TOOL_MARKER,
  type Client,
  type Protocol,
} from "../src/harness";

type CapturedRequest = {
  sequence: number;
  method: string;
  path: string;
  body: Record<string, unknown>;
};

const CLIENTS: Client[] = ["pi", "ai-sdk"];
const PROTOCOLS: Protocol[] = ["responses", "messages"];

function requestList(run: BehaviorRun): CapturedRequest[] {
  return run.requests() as CapturedRequest[];
}

async function waitForRequests(run: BehaviorRun): Promise<CapturedRequest[]> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const requests = requestList(run);
    if (requests.length >= 2) return requests;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(
    `expected two upstream requests, saw ${JSON.stringify(requestList(run), null, 2)}`,
  );
}

function assistantText(message: Record<string, unknown>): string {
  const content = message.content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => {
      if (!part || typeof part !== "object") return "";
      const block = part as Record<string, unknown>;
      return block.type === "text" && typeof block.text === "string"
        ? block.text
        : "";
    })
    .filter(Boolean)
    .join("");
}

function containsValue(value: unknown, needle: string): boolean {
  if (typeof value === "string") return value.includes(needle);
  if (Array.isArray(value)) return value.some((entry) => containsValue(entry, needle));
  if (value && typeof value === "object") {
    return Object.values(value).some((entry) => containsValue(entry, needle));
  }
  return false;
}

for (const client of CLIENTS) {
  for (const inbound of PROTOCOLS) {
    for (const outbound of PROTOCOLS) {
      // Known interoperability gap: the gateway's synthesized Responses
      // response object omits `created_at`, so AI SDK's strict schema rejects
      // the streamed `response.in_progress` frame. Keep the regression visible
      // without changing product code here; removal of `.fails` is the
      // acceptance signal once the event validates.
      const behaviorTest =
        client === "ai-sdk" && inbound === "responses" ? test.fails : test;
      behaviorTest(`${client}: streaming ${inbound} inbound → ${outbound} upstream`, async () => {
        const run = await BehaviorRun.start(inbound, outbound);
        try {
          if (client === "pi") {
            const result = await run.runPi();
            expect(result.exitCode, result.stderr).toBe(0);
            expect(result.stderr).not.toMatch(/error/i);
            expect(result.settled.type).toBe("agent_settled");
            expect(result.settled.aborted).toBe(false);
            expect(result.assistant.role).toBe("assistant");
            expect(result.assistant.stopReason).toBe("stop");
            expect(assistantText(result.assistant)).toBe(FINAL_TEXT);

            const starts = result.tools.filter(
              (event) => event.type === "tool_execution_start",
            );
            const ends = result.tools.filter(
              (event) => event.type === "tool_execution_end",
            );
            expect(starts).toHaveLength(1);
            expect(ends).toHaveLength(1);
            expect(starts[0].toolName).toBe("read");
            expect(String(starts[0].toolCallId)).toContain(CALL_ID);
            expect(ends[0].toolName).toBe("read");
            expect(ends[0].toolCallId).toBe(starts[0].toolCallId);
            expect(ends[0].isError).toBe(false);
          } else {
            const result = await run.runAiSdk();
            expect(result.finishReason).toBe("stop");
            expect(result.text).toBe(FINAL_TEXT);
            expect(result.steps).toHaveLength(2);
            expect(result.steps[0].toolCalls).toHaveLength(1);
            expect(result.steps[0].toolCalls[0].toolName).toBe("read_fixture");
            expect(result.steps[0].toolResults).toHaveLength(1);
            expect(result.steps[0].toolResults[0].toolName).toBe("read_fixture");
            expect(result.steps[0].toolResults[0].output).toEqual({
              marker: TOOL_MARKER,
            });
          }

          const requests = await waitForRequests(run);
          expect(requests).toHaveLength(2);
          expect(requests[0].sequence).toBe(1);
          expect(requests[1].sequence).toBe(2);
          expect(requests[0].method).toBe("POST");
          expect(requests[0].path).toBe(
            outbound === "responses" ? "/responses" : "/messages",
          );
          expect(requests[0].body.model).toBe("test-model");
          expect(requests[0].body.stream).toBe(true);
          expect(requests[1].body.stream).toBe(true);
          expect(Array.isArray(requests[0].body.tools)).toBe(true);
          expect(
            (requests[0].body.tools as unknown[]).length,
          ).toBeGreaterThan(0);
          expect(containsValue(requests[1].body, CALL_ID)).toBe(true);
          expect(containsValue(requests[1].body, TOOL_MARKER)).toBe(true);
          expect(run.upstreamErrors()).toEqual([]);
        } finally {
          await run.dispose();
        }
      });
    }
  }
}
