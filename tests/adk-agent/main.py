"""
A2A server that exposes the ADK agent over HTTP.

Usage:
    python main.py [--host localhost] [--port 10999]

Discovery:
    GET  http://localhost:10999/.well-known/agent.json

A2A endpoint:
    POST http://localhost:10999/
"""

import asyncio
import logging
import os

import click
import uvicorn
from a2a.server.apps import A2AStarletteApplication
from a2a.server.request_handlers import DefaultRequestHandler
from a2a.server.tasks import InMemoryTaskStore
from a2a.types import AgentCapabilities, AgentCard, AgentSkill

from agent import root_agent
from agent_executor import ADKAgentExecutor

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def start_server(host: str, port: int) -> None:
    agent_card = AgentCard(
        name="BitRouter Test Agent",
        description=(
            "A test agent powered by Google ADK. "
            "Uses tools from bitrouter's MCP endpoint and "
            "communicates via A2A protocol."
        ),
        version="0.1.0",
        url=f"http://{host}:{port}",
        default_input_modes=["text", "text/plain"],
        default_output_modes=["text", "text/plain"],
        capabilities=AgentCapabilities(streaming=True),
        skills=[
            AgentSkill(
                id="mcp_tools",
                name="MCP Tool Usage",
                description="Uses tools discovered from bitrouter's MCP endpoint.",
                tags=["mcp", "tools", "bitrouter"],
            ),
        ],
    )

    request_handler = DefaultRequestHandler(
        agent_executor=ADKAgentExecutor(root_agent),
        task_store=InMemoryTaskStore(),
    )

    a2a_app = A2AStarletteApplication(
        agent_card=agent_card,
        http_handler=request_handler,
    )

    config = uvicorn.Config(
        a2a_app.build(),
        host=host,
        port=port,
        log_level="info",
    )
    server = uvicorn.Server(config)
    logger.info("Starting A2A server at http://%s:%d", host, port)
    logger.info("Agent card: http://%s:%d/.well-known/agent.json", host, port)
    mcp_url = os.environ.get("BITROUTER_MCP_URL", "")
    logger.info("MCP source: %s", mcp_url or "(disabled — set BITROUTER_MCP_URL to enable)")
    await server.serve()


@click.command()
@click.option("--host", default="localhost", help="Bind host")
@click.option("--port", default=10999, type=int, help="Bind port")
def run(host: str, port: int) -> None:
    """Start the A2A test agent server."""
    asyncio.run(start_server(host, port))


if __name__ == "__main__":
    run()
