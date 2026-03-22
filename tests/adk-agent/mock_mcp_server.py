"""
Minimal MCP server for testing ADK McpToolset integration.

Speaks MCP JSON-RPC 2.0 over HTTP POST /mcp and SSE GET /mcp/sse.
Provides a single tool 'echo' that returns its input.

Usage: python mock_mcp_server.py [--port 9876]
"""

import asyncio
import json
import logging

import click
import uvicorn
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

PROTOCOL_VERSION = "2025-03-26"
SERVER_NAME = "bitrouter-mock-mcp"

TOOLS = [
    {
        "name": "echo",
        "description": "Echoes back the input message. Use this tool when asked to echo or repeat something.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to echo back",
                }
            },
            "required": ["message"],
        },
    },
    {
        "name": "add",
        "description": "Adds two numbers together. Use this tool for arithmetic addition.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "number", "description": "First number"},
                "b": {"type": "number", "description": "Second number"},
            },
            "required": ["a", "b"],
        },
    },
]


def handle_initialize(req_id):
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {"listChanged": True},
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": "0.1.0",
            },
        },
    }


def handle_tools_list(req_id):
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {"tools": TOOLS},
    }


def handle_tools_call(req_id, params):
    name = params.get("name", "")
    arguments = params.get("arguments", {})

    if name == "echo":
        message = arguments.get("message", "")
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": f"Echo: {message}"}],
            },
        }
    elif name == "add":
        a = arguments.get("a", 0)
        b = arguments.get("b", 0)
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": str(a + b)}],
            },
        }
    else:
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32601,
                "message": f"tool not found: {name}",
            },
        }


def handle_ping(req_id):
    return {"jsonrpc": "2.0", "id": req_id, "result": {}}


async def mcp_endpoint(request: Request):
    try:
        body = await request.json()
    except json.JSONDecodeError:
        return JSONResponse(
            {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "parse error"}},
            status_code=200,
        )

    # Handle notifications (no id)
    if "id" not in body:
        return JSONResponse({}, status_code=202)

    req_id = body.get("id")
    method = body.get("method", "")
    params = body.get("params", {})

    logger.info("MCP request: method=%s id=%s", method, req_id)

    if method == "initialize":
        resp = handle_initialize(req_id)
    elif method == "tools/list":
        resp = handle_tools_list(req_id)
    elif method == "tools/call":
        resp = handle_tools_call(req_id, params)
    elif method == "ping":
        resp = handle_ping(req_id)
    else:
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": f"method not found: {method}"},
        }

    return JSONResponse(resp)


app = Starlette(
    routes=[
        Route("/mcp", mcp_endpoint, methods=["POST"]),
    ]
)


@click.command()
@click.option("--port", default=9876, type=int)
def run(port: int):
    logger.info("Starting mock MCP server on port %d", port)
    logger.info("Tools: %s", [t["name"] for t in TOOLS])
    uvicorn.run(app, host="localhost", port=port, log_level="info")


if __name__ == "__main__":
    run()
