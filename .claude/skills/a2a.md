---
description: Communicate with remote A2A agents — discover capabilities, send tasks, check results, and cancel tasks using the bitrouter CLI
---

# A2A — Communicate with Remote Agents

Use the `bitrouter a2a` CLI to interact with any A2A-compliant remote agent.

## Discover an agent

Fetch a remote agent's capabilities before communicating:

```bash
bitrouter a2a discover https://agent.example.com
```

This fetches the Agent Card and displays the agent's name, skills, interfaces, and capabilities.

## Send a task to an agent

```bash
bitrouter a2a send https://agent.example.com --message "Your request here"
```

This sends a `message/send` JSON-RPC request and waits for the task to complete. The response includes the task ID, status, and any artifacts produced.

## Check task status

If a task is still in progress, check its status:

```bash
bitrouter a2a status https://agent.example.com --task <task-id>
```

## Cancel a task

```bash
bitrouter a2a cancel https://agent.example.com --task <task-id>
```

## Workflow

1. Discover the agent to understand what it can do
2. Send a task with your request
3. If the task doesn't complete immediately, poll with `status`
4. Read the artifacts in the response — these are the agent's output
