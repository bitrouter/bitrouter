#!/usr/bin/env python3
"""Exercise every authenticated control MCP tool over the TLS sidecar."""

from __future__ import annotations

import argparse
import json
import os
import ssl
import urllib.error
import urllib.request


class McpFailure(RuntimeError):
    """The remote MCP endpoint did not satisfy its advertised contract."""


def parse_jsonrpc(body: bytes, content_type: str, expected_id: int) -> dict:
    """Decode either ordinary JSON or the JSON event in an SSE response."""

    text = body.decode("utf-8", "replace").strip()
    payloads: list[str] = []
    if "text/event-stream" in content_type:
        current: list[str] = []
        for line in text.splitlines():
            if not line:
                if current:
                    payloads.append("\n".join(current))
                    current = []
                continue
            if line.startswith("data:"):
                current.append(line[5:].lstrip())
        if current:
            payloads.append("\n".join(current))
    else:
        payloads.append(text)

    for payload in payloads:
        if not payload or payload == "[DONE]":
            continue
        try:
            decoded = json.loads(payload)
        except json.JSONDecodeError as error:
            raise McpFailure(f"MCP response was not JSON: {payload!r}") from error
        candidates = decoded if isinstance(decoded, list) else [decoded]
        for candidate in candidates:
            if candidate.get("id") == expected_id:
                return candidate
    raise McpFailure(f"MCP response did not contain JSON-RPC id {expected_id}: {text!r}")


class McpClient:
    def __init__(self, endpoint: str, token: str, context: ssl.SSLContext) -> None:
        self.endpoint = endpoint
        self.token = token
        self.context = context
        self.next_id = 1
        self.session_id: str | None = None

    def _post(self, payload: dict, expected_id: int | None) -> dict | None:
        headers = {
            "Authorization": f"Bearer {self.token}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session_id is not None:
            headers["Mcp-Session-Id"] = self.session_id
        request = urllib.request.Request(
            self.endpoint,
            data=json.dumps(payload, separators=(",", ":")).encode("utf-8"),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, context=self.context, timeout=10) as response:
                body = response.read()
                self.session_id = response.headers.get("Mcp-Session-Id", self.session_id)
                if expected_id is None:
                    if response.status not in (200, 202):
                        raise McpFailure(f"MCP notification returned HTTP {response.status}")
                    return None
                return parse_jsonrpc(
                    body,
                    response.headers.get("Content-Type", ""),
                    expected_id,
                )
        except urllib.error.HTTPError as error:
            body = error.read().decode("utf-8", "replace")
            raise McpFailure(f"MCP HTTP {error.code}: {body}") from error

    def call(self, method: str, params: dict) -> dict:
        request_id = self.next_id
        self.next_id += 1
        reply = self._post(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params},
            request_id,
        )
        if reply is None:
            raise McpFailure(f"MCP {method} returned no JSON-RPC result")
        if "error" in reply:
            raise McpFailure(f"MCP {method} failed: {reply['error']}")
        return reply["result"]

    def notify(self, method: str, params: dict) -> None:
        self._post({"jsonrpc": "2.0", "method": method, "params": params}, None)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise McpFailure(message)


def structured(result: dict, tool: str) -> dict:
    value = result.get("structuredContent")
    require(isinstance(value, dict), f"{tool} did not return structured content")
    return value


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--endpoint", default="https://server:8443/mcp-control")
    parser.add_argument("--ca", default="/tls/ca.crt")
    parser.add_argument("--token-env", default="CLIENT_READ_TOKEN")
    args = parser.parse_args()

    token = os.environ.get(args.token_env)
    require(token is not None and len(token) >= 32, f"{args.token_env} is not a reader credential")
    client = McpClient(args.endpoint, token, ssl.create_default_context(cafile=args.ca))

    initialized = client.call(
        "initialize",
        {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "remote-administration-acceptance", "version": "1"},
        },
    )
    require(isinstance(initialized.get("protocolVersion"), str), "MCP initialize omitted protocolVersion")
    client.notify("notifications/initialized", {})

    listed = client.call("tools/list", {})
    tools = listed.get("tools")
    require(isinstance(tools, list), "MCP tools/list did not return a tools array")
    names = [tool.get("name") for tool in tools if isinstance(tool, dict)]
    require(all(isinstance(name, str) for name in names), "MCP tools/list returned an unnamed tool")
    require(len(set(names)) == len(names), f"MCP tools/list returned duplicate tools: {names}")

    expected = {"list_models", "route_preview", "status"}
    discovered = set(names)
    require(
        discovered == expected,
        f"MCP tools/list changed without a live-call assertion: {sorted(discovered)}",
    )

    for name in sorted(discovered):
        arguments = {
            "list_models": {"provider": "fixture"},
            "route_preview": {"model": "fixture-a", "prompt": "MCP TLS acceptance"},
            "status": {},
        }[name]
        result = structured(client.call("tools/call", {"name": name, "arguments": arguments}), name)
        if name == "list_models":
            require(
                any(model.get("id") == "fixture-a" for model in result.get("models", [])),
                "list_models did not return fixture-a through the TLS endpoint",
            )
        elif name == "route_preview":
            require(result.get("requested_model") == "fixture-a", "route_preview changed the requested model")
            require(bool(result.get("provider_chain")), "route_preview returned no provider chain")
        else:
            require(result.get("running") is True, "status did not report the running production daemon")
            require(result.get("socket") is None, "status disclosed the server control socket")

    print(json.dumps({"mcp": "passed", "tools": sorted(discovered), "session": client.session_id is not None}))


if __name__ == "__main__":
    main()
