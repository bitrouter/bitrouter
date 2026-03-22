"""
ADK Agent definition with MCP tools sourced from bitrouter.

This agent connects to bitrouter's MCP endpoint to discover and use tools,
and is exposed via A2A protocol for bitrouter to call as an upstream agent.

Set BITROUTER_MCP_URL to enable MCP tool discovery; otherwise the agent
runs standalone with no external tools (useful for testing A2A only).
"""

import os

from google.adk.agents import LlmAgent
from google.adk.tools.mcp_tool.mcp_toolset import McpToolset
from google.adk.tools.mcp_tool.mcp_session_manager import StreamableHTTPConnectionParams

BITROUTER_MCP_URL = os.environ.get("BITROUTER_MCP_URL", "")
LLM_MODEL = os.environ.get("ADK_MODEL", "gemini-2.5-flash")

tools = []
if BITROUTER_MCP_URL:
    mcp_toolset = McpToolset(
        connection_params=StreamableHTTPConnectionParams(url=BITROUTER_MCP_URL),
    )
    tools.append(mcp_toolset)

root_agent = LlmAgent(
    name="bitrouter_test_agent",
    model=LLM_MODEL,
    description="A test agent that uses tools from bitrouter's MCP endpoint.",
    instruction=(
        "You are a helpful assistant. Use the available tools to answer "
        "user questions. Always use the tools available to you when possible. "
        "When asked to echo something, use the echo tool. "
        "When asked to add numbers, use the add tool."
    ),
    tools=tools,
)
