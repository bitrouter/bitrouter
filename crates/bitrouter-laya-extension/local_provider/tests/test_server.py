"""HTTP-process contract tests; fake imports avoid model downloads in CI."""

import http.client
import json
import os
import pathlib
import signal
import socket
import subprocess
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SERVER = ROOT / "server.py"
FAKES = pathlib.Path(__file__).resolve().parent / "fixtures"
MODEL_ID = (
    "convaiinnovations/laya-typed-decisions@"
    "f9ab0b228f0fc0f14d873dbc99038f135c2da1b2"
)
TOKEN = "synthetic-local-secret-1234"
VALID = {
    "model": MODEL_ID,
    "state": {"ticket": "synthetic"},
    "questions": {"approve": {"type": "noul", "instructions": "Approve?"}},
}


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(port, method, path, body=None, token=TOKEN):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        headers = {}
        if token is not None:
            headers["Authorization"] = f"Bearer {token}"
        if body is not None:
            headers["Content-Type"] = "application/json"
            body = json.dumps(body)
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


class LocalProcess(unittest.TestCase):
    def start(self, fake=True, **overrides):
        port = free_port()
        env = os.environ.copy()
        env["LAYA_LOCAL_TOKEN"] = TOKEN
        env["LAYA_OFFLINE"] = "1"
        if fake:
            env["PYTHONPATH"] = str(FAKES)
        else:
            env.pop("PYTHONPATH", None)
        env.update(overrides)
        python = os.environ.get("LAYA_PYTHON", "python3")
        process = subprocess.Popen(
            [python, "-B", str(SERVER), "--port", str(port)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.addCleanup(self.stop, process)
        return process, port

    def ready(self, process, port, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if process.poll() is not None:
                stdout, stderr = process.communicate(timeout=2)
                self.fail(
                    f"local provider exited {process.returncode}: "
                    f"{stdout.decode()} {stderr.decode()}"
                )
            try:
                status, body = request(port, "GET", "/health")
                self.assertEqual(status, 200)
                self.assertEqual(body["model"], MODEL_ID)
                return
            except (ConnectionError, OSError, http.client.HTTPException):
                time.sleep(0.05)
        self.fail("local provider did not become ready")

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.terminate()
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=5)

    def test_readiness_auth_bounds_inference_and_shutdown(self):
        process, port = self.start()
        self.ready(process, port)
        status, _ = request(port, "POST", "/v1/decisions", VALID, token=None)
        self.assertEqual(status, 401)
        status, _ = request(port, "POST", "/v1/decisions", VALID, token="wrong")
        self.assertEqual(status, 401)
        wrong_model = {**VALID, "model": "moving-alias"}
        status, _ = request(port, "POST", "/v1/decisions", wrong_model)
        self.assertEqual(status, 422)
        many = {**VALID, "questions": {f"q{i}": VALID["questions"]["approve"] for i in range(17)}}
        status, _ = request(port, "POST", "/v1/decisions", many)
        self.assertEqual(status, 422)
        choices = {f"c{i}": None for i in range(21)}
        too_many_options = {**VALID, "questions": {"choice": {"type": "choice", "instructions": "Pick", "criteria": choices}}}
        status, _ = request(port, "POST", "/v1/decisions", too_many_options)
        self.assertEqual(status, 422)
        too_many_levels = {**VALID, "questions": {"severity": {"type": "score", "instructions": "Rate", "criteria": ["level"] * 11}}}
        status, _ = request(port, "POST", "/v1/decisions", too_many_levels)
        self.assertEqual(status, 422)
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        try:
            connection.putrequest("POST", "/v1/decisions")
            connection.putheader("Authorization", f"Bearer {TOKEN}")
            connection.putheader("Content-Length", str(1024 * 1024 + 1))
            connection.endheaders()
            self.assertEqual(connection.getresponse().status, 413)
        finally:
            connection.close()
        status, body = request(port, "POST", "/v1/decisions", VALID)
        self.assertEqual(status, 200)
        self.assertEqual(body["model"], MODEL_ID)
        self.assertEqual(body["answers"]["approve"]["noul"], 0.75)
        self.assertEqual(body["usage"], {"input_tokens": 12, "output_tokens": 0})
        self.assertNotIn("cost", body["usage"])
        process.send_signal(signal.SIGTERM)
        self.assertEqual(process.wait(timeout=5), 0)

    def test_startup_failure_does_not_open_health_socket_or_leak_error(self):
        process, port = self.start(FAKE_LAYA_FAIL_START="1")
        _, stderr = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 1)
        self.assertNotIn(b"private startup diagnostic", stderr)
        with self.assertRaises((ConnectionError, OSError)):
            request(port, "GET", "/health")

    def test_inference_failure_has_safe_status_and_body(self):
        process, port = self.start(FAKE_LAYA_FAIL_INFER="1")
        self.ready(process, port)
        status, body = request(port, "POST", "/v1/decisions", VALID)
        self.assertEqual(status, 502)
        self.assertEqual(body, {"error": {"code": "model_failed"}})
        self.assertNotIn("private inference diagnostic", str(body))

    @unittest.skipUnless(os.environ.get("LAYA_REAL_SMOKE") == "1", "opt-in real checkpoint")
    def test_real_checkpoint_process(self):
        process, port = self.start(fake=False)
        self.ready(process, port, timeout=120)
        request_body = {
            "model": MODEL_ID,
            "state": {"ticket": "Synthetic duplicate charge"},
            "questions": {
                "billing": {"type": "noul", "instructions": "Is this about billing?"},
                "team": {"type": "choice", "instructions": "Which team?", "criteria": {"billing": None, "technical": None}},
                "urgency": {"type": "score", "instructions": "How urgent?", "criteria": ["routine", "urgent"]},
            },
        }
        status, body = request(port, "POST", "/v1/decisions", request_body)
        self.assertEqual(status, 200)
        self.assertEqual(body["model"], MODEL_ID)
        self.assertEqual({k: v["type"] for k, v in body["answers"].items()},
                         {"billing": "noul", "team": "choice", "urgency": "score"})
        self.assertGreater(body["usage"]["input_tokens"], 0)
        self.assertEqual(body["usage"]["output_tokens"], 0)
        self.assertNotIn("cost", body["usage"])
