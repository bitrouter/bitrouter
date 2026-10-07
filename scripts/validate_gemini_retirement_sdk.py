"""Run the actual Antigravity SDK through an isolated BitRouter fixture gateway.

Requires Python >=3.10 and google-antigravity==0.1.20. This proves SDK/client
execution with a fixture model, not live Google inference. --signed reproduces
and verifies the known SDK continuity limitation without bypassing admission.
"""

import argparse
import asyncio
import importlib.metadata
import json
import os
import pathlib
import socket
import sys
import tempfile
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from google.antigravity import (
    Agent,
    BuiltinTools,
    CapabilitiesConfig,
    LocalOpenAIAgentConfig,
)
from google.antigravity.hooks import policy
from google.antigravity.types import AntigravityExecutionError

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--bro",
    type=pathlib.Path,
    default=pathlib.Path(__file__).resolve().parents[1] / "target/debug/bro",
)
parser.add_argument("--signed", action="store_true")
options = parser.parse_args()
requests = []
gateway_requests = []
router_port = 0
executed = []


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append(body)
        index = len(requests)
        tools = body.get("tools", [])
        functions = [
            t["function"]["name"] for t in tools if t.get("type") == "function"
        ]
        if index == 1:
            name = next(n for n in functions if "lookup_nonce" in n)
            arguments = json.dumps(
                {
                    "query": "review-fixture",
                    "toolAction": "Reading fixture",
                    "toolSummary": "Fixture check",
                },
                separators=(",", ":"),
            )
            call = {
                "id": "call_sdk",
                "type": "function",
                "function": {"name": name, "arguments": arguments},
                "extra_content": {
                    "google": {"thought_signature": "c2RrLWZpeHR1cmUtc2lnbmF0dXJl"}
                },
            }
            if not options.signed:
                call.pop("extra_content")
            message = {"role": "assistant", "content": None, "tool_calls": [call]}
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": "verified nonce: fixture-4356"}
            finish = "stop"
        response = {
            "id": "chatcmpl-fixture",
            "object": "chat.completion",
            "model": body["model"],
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19},
        }
        self.send_response(200)
        if body.get("stream"):
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            delta = message.copy()
            delta.pop("role", None)
            if "tool_calls" in delta:
                for i, tc in enumerate(delta["tool_calls"]):
                    tc["index"] = i
            chunk = {
                "id": "chatcmpl-fixture",
                "object": "chat.completion.chunk",
                "model": body["model"],
                "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
            }
            terminal = {
                "id": "chatcmpl-fixture",
                "object": "chat.completion.chunk",
                "model": body["model"],
                "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                "usage": response["usage"],
            }
            self.wfile.write(
                (
                    "data: "
                    + json.dumps(chunk)
                    + "\n\ndata: "
                    + json.dumps(terminal)
                    + "\n\ndata: [DONE]\n\n"
                ).encode()
            )
        else:
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(response).encode())


class GatewayProxy(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        gateway_requests.append(json.loads(raw))
        request = urllib.request.Request(
            f"http://127.0.0.1:{router_port}" + self.path,
            data=raw,
            headers={"Content-Type": "application/json"},
        )
        try:
            response = urllib.request.urlopen(request, timeout=20)
        except urllib.error.HTTPError as error:
            response = error
        self.send_response(response.status)
        self.send_header(
            "Content-Type", response.headers.get("Content-Type", "application/json")
        )
        self.end_headers()
        self.wfile.write(response.read())


def lookup_nonce(query: str) -> str:
    """Read the fixture nonce for the supplied query."""
    executed.append(query)
    return "fixture-4356"


async def main(root, log):
    global router_port
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    router_port = port
    gateway = ThreadingHTTPServer(("127.0.0.1", 0), GatewayProxy)
    threading.Thread(target=gateway.serve_forever, daemon=True).start()
    config = root / "bitrouter.yaml"
    config.write_text(
        json.dumps(
            {
                "inherit_defaults": False,
                "registry": {"enabled": False},
                "server": {"listen": f"127.0.0.1:{port}", "skip_auth": True},
                "database": {"url": "sqlite::memory:"},
                "providers": {
                    "fixture": {
                        "api_base": f"http://127.0.0.1:{upstream.server_port}",
                        "api_key": "sdk-fixture-static-key",
                        "api_protocol": [{"*": "chat_completions"}],
                        "models": [
                            {
                                "id": "sdk-test",
                                "capabilities": ["tools"],
                                "compatibility": {
                                    "chat_completions": {
                                        "google_extensions": options.signed
                                    }
                                },
                            }
                        ],
                    }
                },
            }
        )
    )
    env = os.environ.copy()
    env["BITROUTER_HOME"] = str(root)
    env["XDG_CACHE_HOME"] = str(root / "cache")
    bro = await asyncio.create_subprocess_exec(
        str(options.bro.resolve()),
        "serve",
        "--config",
        str(config),
        cwd=root,
        env=env,
        stdout=log,
        stderr=log,
    )
    try:
        for _ in range(100):
            if bro.returncode is not None:
                log.seek(0)
                raise RuntimeError(log.read()[-2000:])
            try:
                await asyncio.to_thread(probe, f"http://127.0.0.1:{port}/v1/models")
                break
            except (OSError, urllib.error.URLError):
                await asyncio.sleep(0.1)
        agent_config = LocalOpenAIAgentConfig(
            model="sdk-test",
            base_url=f"http://127.0.0.1:{gateway.server_port}/v1",
            tools=[lookup_nonce],
            capabilities=CapabilitiesConfig(
                enable_subagents=False, enabled_tools=[BuiltinTools.FINISH]
            ),
            policies=[policy.allow_all()],
            workspaces=[str(root)],
            save_dir=str(root / "sdk-sessions"),
            app_data_dir=str(root / "sdk-app"),
        )

        async def task():
            async with Agent(agent_config) as agent:
                response = await agent.chat(
                    "Call lookup_nonce once with query review-fixture and report the returned nonce."
                )
                output = ""
                async for token in response:
                    output += str(token)
                return output

        failure = None
        try:
            output = await asyncio.wait_for(task(), 45)
        except (TimeoutError, AntigravityExecutionError) as error:
            failure = error
        if options.signed:
            assert failure is not None, (
                "SDK continuity behavior changed; review the signed-tool evidence"
            )
            assert len(requests) == 1 and len(gateway_requests) == 2, (
                "Signed replay reached upstream or failed before exercising the tool cycle"
            )
            calls = [
                call
                for message in gateway_requests[-1]["messages"]
                for call in message.get("tool_calls", [])
            ]
            assert calls and not calls[0].get("extra_content", {}).get(
                "bitrouter", {}
            ).get("google_replay_proof"), (
                "SDK no longer drops replay proof; re-evaluate compatibility"
            )
            status = "signed Google tool continuity unsupported; replay rejected before second upstream call"
        else:
            if failure is not None:
                raise failure
            assert len(requests) == 2 and executed == ["review-fixture"], (
                "Tool execution/cardinality mismatch"
            )
            assert "fixture-4356" in output, (
                "Final response did not contain the tool result"
            )
            assert all(request.get("stream") for request in requests), (
                "Expected streamed SDK requests"
            )
            status = "unsigned Chat tool task completed"
        print(
            json.dumps(
                {
                    "sdk_version": importlib.metadata.version("google-antigravity"),
                    "python": sys.version.split()[0],
                    "status": status,
                    "upstream_calls": len(requests),
                    "gateway_calls": len(gateway_requests),
                    "custom_tool_calls": len(executed),
                    "streamed": [request.get("stream", False) for request in requests],
                },
                indent=2,
            )
        )

    finally:
        if bro.returncode is None:
            bro.terminate()
        try:
            await asyncio.wait_for(bro.wait(), 5)
        except TimeoutError:
            bro.kill()
            await bro.wait()
        await asyncio.to_thread(upstream.shutdown)
        await asyncio.to_thread(gateway.shutdown)


def probe(url):
    with urllib.request.urlopen(url, timeout=0.2):
        return


def run():
    with tempfile.TemporaryDirectory(prefix="bitrouter-sdk-") as directory:
        root = pathlib.Path(directory)
        with (root / "bro.log").open("w+") as log:
            asyncio.run(main(root, log))


if __name__ == "__main__":
    run()
