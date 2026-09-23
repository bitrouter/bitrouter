"""Loopback-only HTTP process for one pinned Laya Typed Decisions checkpoint.

This process is deliberately separate from BitRouter. It loads the model once
before opening its health and inference socket. BitRouter alone owns public
authentication, routing, retries, and metering.
"""

import argparse
import hmac
import json
import os
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

MODEL_REPO = "convaiinnovations/laya-typed-decisions"
MODEL_REVISION = "f9ab0b228f0fc0f14d873dbc99038f135c2da1b2"
MODEL_ID = f"{MODEL_REPO}@{MODEL_REVISION}"
MAX_BODY_BYTES = 1024 * 1024
MAX_QUESTIONS = 16
MAX_CHOICE_OPTIONS = 20
MAX_SCORE_LEVELS = 10
SNAPSHOT_FILES = [
    "rl_agent_config.json",
    "model.safetensors",
    "tokenizer/*",
    "encoder/*",
]


def load_agent():
    from huggingface_hub import snapshot_download
    import laya

    snapshot = snapshot_download(
        MODEL_REPO,
        revision=MODEL_REVISION,
        allow_patterns=SNAPSHOT_FILES,
        local_files_only=os.environ.get("LAYA_OFFLINE") == "1",
    )
    return laya.load(snapshot)


def structured(value):
    return isinstance(value, (str, dict, list))


def valid_request(body):
    if not isinstance(body, dict) or set(body) != {"model", "state", "questions"}:
        return False
    if body["model"] != MODEL_ID or not structured(body["state"]):
        return False
    questions = body["questions"]
    if not isinstance(questions, dict) or not 1 <= len(questions) <= MAX_QUESTIONS:
        return False
    for question_id, question in questions.items():
        if not isinstance(question_id, str) or not question_id:
            return False
        if not isinstance(question, dict) or not structured(question.get("instructions")):
            return False
        kind = question.get("type")
        criteria = question.get("criteria")
        if kind == "noul":
            if set(question) - {"type", "instructions", "criteria"}:
                return False
            if criteria is not None and (
                not isinstance(criteria, dict)
                or set(criteria) != {"true", "false"}
                or not all(structured(value) for value in criteria.values())
            ):
                return False
        elif kind == "choice":
            if set(question) != {"type", "instructions", "criteria"}:
                return False
            if not isinstance(criteria, dict) or not 2 <= len(criteria) <= MAX_CHOICE_OPTIONS:
                return False
            if not all(
                isinstance(name, str)
                and name
                and (value is None or structured(value))
                for name, value in criteria.items()
            ):
                return False
        elif kind == "score":
            if set(question) != {"type", "instructions", "criteria"}:
                return False
            if not isinstance(criteria, list) or not 2 <= len(criteria) <= MAX_SCORE_LEVELS:
                return False
            if not all(structured(value) for value in criteria):
                return False
        else:
            return False
    return True


def serve(agent, token, port):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, _format, *_args):
            # Neither request content nor the bearer token belongs in logs.
            pass

        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def respond(self, status, body):
            encoded = json.dumps(body, allow_nan=False, separators=(",", ":")).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(encoded)

        def do_GET(self):
            if self.path == "/health":
                self.respond(200, {"status": "ready", "model": MODEL_ID})
            else:
                self.respond(404, {"error": {"code": "not_found"}})

        def do_POST(self):
            if self.path != "/v1/decisions":
                self.respond(404, {"error": {"code": "not_found"}})
                return
            supplied = self.headers.get("Authorization", "")
            if not hmac.compare_digest(supplied, f"Bearer {token}"):
                self.respond(401, {"error": {"code": "unauthorized"}})
                return
            try:
                length = int(self.headers.get("Content-Length", ""))
            except ValueError:
                self.respond(400, {"error": {"code": "invalid_body"}})
                return
            if length < 1:
                self.respond(400, {"error": {"code": "invalid_body"}})
                return
            if length > MAX_BODY_BYTES:
                self.respond(413, {"error": {"code": "body_too_large"}})
                return
            try:
                body = json.loads(self.rfile.read(length))
            except (ValueError, UnicodeDecodeError, TimeoutError):
                self.respond(400, {"error": {"code": "invalid_body"}})
                return
            if not valid_request(body):
                self.respond(422, {"error": {"code": "invalid_evaluation"}})
                return
            try:
                output = agent.system_one(body["state"], body["questions"])
                result = {
                    "model": MODEL_ID,
                    "answers": output["answers"],
                    "usage": output["usage"],
                }
                self.respond(200, result)
            except Exception:
                self.respond(502, {"error": {"code": "model_failed"}})

    server = HTTPServer(("127.0.0.1", port), Handler)

    def shutdown(_signum, _frame):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    try:
        server.serve_forever(poll_interval=0.05)
    finally:
        server.server_close()


def main():
    parser = argparse.ArgumentParser(description="Pinned local Laya evaluation provider")
    parser.add_argument("--port", type=int, default=8767)
    args = parser.parse_args()
    token = os.environ.get("LAYA_LOCAL_TOKEN", "")
    if len(token) < 16 or not 1 <= args.port <= 65535:
        print("Laya requires a local bearer token and valid port", file=sys.stderr)
        return 2
    try:
        agent = load_agent()
        serve(agent, token, args.port)
    except Exception as error:
        print(f"Laya startup failed: {type(error).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
