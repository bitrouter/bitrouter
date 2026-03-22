# BitRouter Test Agent (Google ADK + A2A + MCP)

A minimal agent for testing bitrouter's MCP and A2A endpoints using Google ADK.

## Architecture

```
bitrouter (A2A client)                   bitrouter (MCP server)
    │                                         ▲
    │ A2A JSON-RPC                            │ MCP (SSE)
    ▼                                         │
┌─────────────────────────────────────────────┐
│  ADK Test Agent (this project)              │
│  - A2A server on :10999                     │
│  - MCP client → bitrouter :8787/mcp/sse     │
│  - LLM: Gemini 2.5 Flash                   │
└─────────────────────────────────────────────┘
```

## Setup

```bash
cd tests/adk-agent
python -m venv .venv
source .venv/bin/activate
pip install -e .
```

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `BITROUTER_MCP_URL` | `http://localhost:8787/mcp/sse` | bitrouter's MCP SSE endpoint |
| `ADK_MODEL` | `gemini-2.5-flash` | LLM model for the agent |
| `GOOGLE_API_KEY` | (required) | Gemini API key |

## Run

```bash
# Start bitrouter first
cd ../.. && cargo run -- serve

# Then start the test agent
cd tests/adk-agent
source .venv/bin/activate
export GOOGLE_API_KEY="your-key"
python main.py --port 10999
```

## Test Endpoints

```bash
# Agent card discovery
curl http://localhost:10999/.well-known/agent.json

# Send a message via A2A
curl -X POST http://localhost:10999/ \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": "1",
    "method": "message/send",
    "params": {
      "message": {
        "role": "user",
        "parts": [{"kind": "text", "text": "Hello, what tools do you have?"}],
        "messageId": "msg-1"
      }
    }
  }'
```

## Configure bitrouter to use this agent

Add to your `bitrouter.yaml`:

```yaml
a2a_agents:
  - name: "test-agent"
    url: "http://localhost:10999"
```
