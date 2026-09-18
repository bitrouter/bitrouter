#!/usr/bin/env python3
"""Exercise built bro/checker executables against a counted local mock upstream.

Uses only Python's standard library. No model provider or user daemon is used.
Run after building both binaries; --output preserves logs and a JSON report.
"""

import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def post_json(url, body, request_id=None):
    headers = {"content-type": "application/json"}
    if request_id:
        headers["x-bitrouter-request-id"] = request_id
    request = urllib.request.Request(url, json.dumps(body).encode(), headers)
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


class Upstream(http.server.BaseHTTPRequestHandler):
    calls = []
    lock = threading.Lock()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path == "/invalid":
            body = b'{"decision":"allow"}'
        elif self.path == "/slow":
            time.sleep(0.5)
            body = b'{}'
        else:
            with self.lock:
                self.calls.append(data)
            message = {
                "id": "local-fixture", "object": "chat.completion", "model": "model",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3},
            }
            body = json.dumps(message).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.wfile.write(body)


def run(args):
    bro, checker = str(Path(args.bro).resolve()), str(Path(args.checker).resolve())
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    results = []
    with tempfile.TemporaryDirectory(prefix="brg-", dir="/tmp") as directory:
        home = Path(directory)
        env = dict(os.environ, BITROUTER_HOME=str(home), GUARDRAILS_E2E_TOKEN="local-test-token")
        processes, logs = [], []

        def launch(binary, arguments, name):
            log = (output / f"{name}.log").open("w")
            logs.append(log)
            child = subprocess.Popen([binary, *arguments], cwd=home, env=env,
                                     stdout=log, stderr=subprocess.STDOUT)
            processes.append(child)
            return child

        def stop(child):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)

        def wait_for(child, number):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                require(child.poll() is None, f"process exited: {child.args}")
                try:
                    with socket.create_connection(("127.0.0.1", number), timeout=0.1):
                        return
                except OSError:
                    time.sleep(0.05)
            raise TimeoutError(f"listener {number} did not start")

        def cli(*arguments):
            completed = subprocess.run([bro, *arguments], cwd=home, env=env,
                                       capture_output=True, timeout=20, text=True)
            require(completed.returncode == 0, f"{arguments}: {completed.stderr} {completed.stdout}")
            return json.loads(completed.stdout)

        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        try:
            checker_ports = [port(), port()]
            checker_children = []
            for index, pattern in enumerate(["NEVER_MATCH_FIXTURE", "forbidden\\nvalue"]):
                rules = home / f"rules-{index}.yaml"
                rules.write_text(json.dumps({"scope": "input", "rules": [{"name": "fixture", "pattern": pattern,
                                                                          "action": "block"}]}))
                child = launch(checker, ["--listen", f"127.0.0.1:{checker_ports[index]}",
                                        "--rules", str(rules), "--credential-env", "GUARDRAILS_E2E_TOKEN"],
                               f"checker-{index}")
                checker_children.append(child)
                wait_for(child, checker_ports[index])
            denied, _ = post_json(f"http://127.0.0.1:{checker_ports[0]}/check", {})
            require(denied == 401, "service accepted missing bearer credential")
            results.append("service requires configured bearer credential")

            base = f"http://127.0.0.1:{upstream.server_port}"
            checks = {
                "open": {"endpoint": f"http://127.0.0.1:{checker_ports[0]}/check",
                         "credential_env": "GUARDRAILS_E2E_TOKEN"},
                "restricted": {"endpoint": f"http://127.0.0.1:{checker_ports[1]}/check",
                               "credential_env": "GUARDRAILS_E2E_TOKEN"},
                "malformed": {"endpoint": base + "/invalid"},
                "timeout": {"endpoint": base + "/slow"},
            }
            for definition in checks.values():
                definition["contract_version"] = 1
            routers = {name: {"selection": {"kind": "model", "model": "fixture:model"},
                              "checks": {"request": [{"checker": name, "timeout_ms": 80 if name == "timeout" else 5000}]}}
                       for name in checks}
            gateway_port = port()
            config = {"inherit_defaults": False,
                      "server": {"listen": f"127.0.0.1:{gateway_port}", "skip_auth": True},
                      "database": {"url": "sqlite::memory:"},
                      "providers": {"fixture": {"api_base": base, "api_key": "upstream-secret",
                                                 "models": [{"id": "model"}]}},
                      "routers": routers, "checkers": checks}
            config_path = home / "bitrouter.yaml"
            config_path.write_text(json.dumps(config))
            daemon = launch(bro, ["serve", "--config", str(config_path)], "daemon")
            wait_for(daemon, gateway_port)
            before = cli("checks")
            probe = cli("checks", "probe", "restricted")
            require(probe["synthetic"] and not probe["counts_as_usage"], "probe claims real usage")
            require(all(binding.get("last_actual") is None for row in cli("checks")["checkers"]
                        for binding in row["bindings"]), "probe changed actual usage")
            results.append("synthetic probe is separate from actual use")

            def request(router, identity, stream=False):
                payload = {"model": "bitrouter/" + router, "stream": stream,
                           "messages": [{"role": "user", "content": "forbidden"},
                                        {"role": "user", "content": "value"}]}
                return post_json(f"http://127.0.0.1:{gateway_port}/v1/chat/completions", payload, identity)

            status, body = request("open", "allowed")
            require(status == 200, f"allow failed: {status} {body}")
            require(len(Upstream.calls) == 1, "allow did not dispatch exactly once")
            for router, outcome in [("restricted", "denied"), ("malformed", "failed"), ("timeout", "failed")]:
                for stream in [False, True]:
                    identity = f"{router}-{stream}"
                    status, _ = request(router, identity, stream)
                    require(status >= 400, f"{identity} unexpectedly allowed")
                    receipt = cli("checks", "receipt", identity)["receipt"]
                    require(receipt["identity"]["router_id"] == router, "router identity changed")
                    require(not receipt["upstream_started"] and receipt["outcome"] == outcome,
                            f"unexpected failure evidence: {receipt}")
                    require(len(Upstream.calls) == 1, f"{identity} dispatched upstream")
            results.append("two service bindings; cross-fragment deny, malformed and timeout have zero upstream calls in both modes")
            allowed = cli("checks", "receipt", "allowed")["receipt"]
            require(allowed["upstream_started"] and allowed["outcome"] == "completed", "allow receipt wrong")
            require("upstream-secret" not in json.dumps(cli("checks", "receipts")), "receipt leaked credential")
            results.append("receipts remain queryable without telemetry exporter")

            stop(checker_children[0])
            status, _ = request("open", "unavailable")
            require(status >= 400 and len(Upstream.calls) == 1, "unavailable checker failed open")
            results.append("stopped checker fails closed")
            config["checkers"]["open"]["endpoint"] += "-changed"
            config_path.write_text(json.dumps(config))
            state = cli("checks")["config_state"]
            require(state["restart_required_fields"], f"saved changes reported running: {state}")
            results.append("saved checker changes require restart")
            config["checkers"]["open"]["endpoint"] = checks["open"]["endpoint"].removesuffix("-changed")
            config_path.write_text(json.dumps(config))
            stop(daemon)
            daemon = launch(bro, ["serve", "--config", str(config_path)], "daemon-restarted")
            wait_for(daemon, gateway_port)
            after = cli("checks")
            require(before["receipt_retention"]["incarnation_id"] != after["receipt_retention"]["incarnation_id"],
                    "restart reused incarnation")
            require(cli("checks", "receipt", "allowed")["status"] == "unknown", "receipt persisted across restart")
            results.append("restart resets process-local receipts and incarnation")
            report = {"passed": results, "upstream_calls": len(Upstream.calls),
                      "provider": "local counted mock; no real model calls", "bro": bro, "checker": checker}
            (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report, indent=2))
        finally:
            for child in reversed(processes):
                stop(child)
            upstream.shutdown()
            upstream.server_close()
            for log in logs:
                log.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bro", required=True)
    parser.add_argument("--checker", required=True)
    parser.add_argument("--output", required=True)
    run(parser.parse_args())
