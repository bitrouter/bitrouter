#!/usr/bin/env python3
"""Small deterministic OpenAI-compatible upstream for container acceptance."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import time


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format, *_args):
        # The harness needs deterministic logs, not one line per synthetic request.
        return

    def respond(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            self.respond(200, {"ok": True})
            return
        self.respond(404, {"error": {"message": "not found"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            request = json.loads(raw)
        except json.JSONDecodeError:
            self.respond(400, {"error": {"message": "invalid JSON"}})
            return

        if self.path != "/v1/chat/completions":
            self.respond(404, {"error": {"message": "not found"}})
            return

        messages = request.get("messages", [])
        content = " ".join(
            message.get("content", "")
            for message in messages
            if isinstance(message, dict) and isinstance(message.get("content", ""), str)
        )
        if "force-rate-limit" in content:
            self.respond(
                429,
                {
                    "error": {
                        "message": "rate limit fixture-upstream-secret",
                        "type": "rate_limit_error",
                    }
                },
            )
            return

        model = request.get("model", "fixture-a")
        self.respond(
            200,
            {
                "id": "chatcmpl-remote-administration",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "fixture response"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            },
        )


ThreadingHTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
