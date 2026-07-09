---
title: Tools
description: Tools are the capabilities an agent acquires at runtime — MCP servers and Agent Skills — both served through one BitRouter gateway.
sourceHash: 5db598556756983832c0a835dcdcb78f53cd8aa6ad1d70a117cfe1a84ddb5e32
---

On BitRouter, **tools** are the capabilities an agent picks up at runtime to get work done. They come in two kinds, and BitRouter serves both through one endpoint so an agent connects once instead of wiring up each source by hand:

- **MCP servers** — callable tools (a search API, a database, a file system, a payment rail).
- **Agent Skills** — loadable know-how: procedures and instructions an agent pulls in on demand.

## The MCP gateway

An agent's callable tools are [MCP](https://modelcontextprotocol.io) servers. Each exposes a set of tools an agent can discover and invoke. The catch is that those servers live in many places, each with its own address, auth, and endpoint — an agent that wants ten tool servers normally has to know about ten endpoints.

BitRouter's **MCP gateway** sits in front of them and proxies them. Your agent connects to one BitRouter endpoint; behind it, the gateway forwards **tool discovery** and **tool calls** to the right upstream host and relays the responses back. One connection, many tool servers — mirroring how BitRouter treats models.

Routing tools through one gateway buys you three things you'd otherwise rebuild per agent:

- **Uniform auth** — the agent authenticates once to BitRouter, instead of carrying credentials for every upstream server.
- **Discovery** — tools across hosts surface in one place, so an agent finds what's available without being pre-wired to each server.
- **Policy** — every tool call passes through the gateway, the natural place to enforce rules consistently.

### One endpoint, many servers

The gateway exposes a single aggregate endpoint that **fans out** across every configured server. A list call queries each server and returns the merged catalog; a tool call routes to the owning server. To keep names from colliding, each server's tools are **namespaced with a prefix** — the `search` tool on the `demo` server is advertised as `demo__search`. A server can **opt out** of the aggregate to stay reachable only on its own route, and cheap list calls are **cached** briefly so discovery stays fast. The prefix and cache settings are configurable per server.

## Agent Skills

The second capability an agent acquires is **know-how**. **Agent Skills** are drop-in capabilities an agent loads on demand — a packaged procedure, a set of instructions, a workflow — discovered and pulled in the same way tools are, through the gateway. Where an MCP tool is something the agent *calls*, a Skill is something the agent *learns*: the knowledge travels with the agent instead of living in a human's setup notes. One common Skill simply teaches an agent how to drive BitRouter itself.

## Learn how to

- [Server tools](/docs/features/server-tools) — let BitRouter run the tool-calling loop for you, server-side.
- [Toolsets](/docs/features/server-tools#how-tools-are-bundled-toolsets) — how the tools advertised on a request are bundled and namespaced.
