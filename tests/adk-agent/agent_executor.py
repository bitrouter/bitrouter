"""
Bridges the A2A protocol to ADK Runner.

Translates incoming A2A requests into ADK agent invocations and streams
results back as A2A task events.
"""

import logging
import uuid

from a2a.server.agent_execution import AgentExecutor, RequestContext
from a2a.server.events import EventQueue
from a2a.server.tasks import TaskUpdater
from a2a.types import Part, TaskState, TextPart
from a2a.utils import new_agent_text_message, new_task
from google.adk.runners import Runner
from google.adk.sessions import InMemorySessionService
from google.genai import types

logger = logging.getLogger(__name__)


class ADKAgentExecutor(AgentExecutor):
    """Wraps an ADK LlmAgent as an A2A-compatible executor."""

    def __init__(self, agent):
        self.agent = agent
        self.runner = Runner(
            app_name=agent.name,
            agent=agent,
            session_service=InMemorySessionService(),
        )

    async def execute(
        self,
        context: RequestContext,
        event_queue: EventQueue,
    ) -> None:
        query = context.get_user_input()
        task = context.current_task or new_task(context.message)
        await event_queue.enqueue_event(task)

        updater = TaskUpdater(event_queue, task.id, task.context_id)
        await updater.update_status(
            TaskState.working,
            new_agent_text_message("Processing...", task.context_id, task.id),
        )

        # Use a unique session_id per request to avoid collisions
        session_id = str(uuid.uuid4())
        session = await self.runner.session_service.create_session(
            app_name=self.agent.name,
            user_id=task.context_id,
            session_id=session_id,
        )

        content = types.Content(
            role="user",
            parts=[types.Part.from_text(text=query)],
        )

        # Collect text from all events with content (not just final)
        # because some models emit content in non-final events
        response_text = ""
        async for event in self.runner.run_async(
            user_id=task.context_id,
            session_id=session.id,
            new_message=content,
        ):
            if not event.content or not event.content.parts:
                continue
            for part in event.content.parts:
                if hasattr(part, "text") and part.text:
                    response_text += part.text

        if not response_text:
            response_text = "(no response from agent)"

        await updater.add_artifact(
            [Part(root=TextPart(text=response_text))],
            name="response",
        )
        await updater.complete()

    async def cancel(
        self,
        context: RequestContext,
        event_queue: EventQueue,
    ) -> None:
        raise NotImplementedError("Cancellation not supported")
