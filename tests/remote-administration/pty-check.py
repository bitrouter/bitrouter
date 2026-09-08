#!/usr/bin/env python3
"""Real PTY acceptance for the remote dashboard; standard library only.

Uses a bounded virtual terminal and observable state conditions, not timing
sleeps. Tests refresh without mutation, explicit reload rendering, clean quit,
terminal restoration, and subsequent shell output on the same PTY.
"""
import argparse
import codecs
import fcntl
import json
import os
import pty
import re
import select
import ssl
import struct
import subprocess
import termios
import time
import urllib.parse
import urllib.request


class Screen:
    def __init__(self, rows=60, cols=180):
        self.rows, self.cols = rows, cols
        self.cells = [[" "] * cols for _ in range(rows)]
        self.row = self.col = 0
        self.pending = ""
        self.responses = bytearray()
        self.synchronized_text = None
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        self.pending = ""
        index = 0
        while index < len(text):
            char = text[index]
            if char == "\x1b":
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", text[index:])
                if match:
                    raw, _, command = match.groups()
                    values = [int(value) if value else 0 for value in raw.lstrip("?<=>").split(";")]
                    first = values[0] or 1
                    if command in "Hf":
                        self.row = min(self.rows - 1, first - 1)
                        self.col = min(self.cols - 1, (values[1] or 1) - 1 if len(values) > 1 else 0)
                    elif command == "G":
                        self.col = min(self.cols - 1, first - 1)
                    elif command == "d":
                        self.row = min(self.rows - 1, first - 1)
                    elif command in "ABCD":
                        self.row = max(0, min(self.rows - 1, self.row + (first if command == "B" else -first if command == "A" else 0)))
                        self.col = max(0, min(self.cols - 1, self.col + (first if command == "C" else -first if command == "D" else 0)))
                    elif command == "J":
                        if values[0] in (2, 3):
                            self.cells = [[" "] * self.cols for _ in range(self.rows)]
                        elif values[0] == 0:
                            self.cells[self.row][self.col:] = [" "] * (self.cols - self.col)
                            for row in range(self.row + 1, self.rows):
                                self.cells[row] = [" "] * self.cols
                        elif values[0] == 1:
                            for row in range(self.row):
                                self.cells[row] = [" "] * self.cols
                            self.cells[self.row][:self.col + 1] = [" "] * (self.col + 1)
                    elif command == "h" and raw == "?2026":
                        self.synchronized_text = self.text()
                    elif command == "l" and raw == "?2026":
                        self.synchronized_text = None
                    elif command == "n" and raw == "6":
                        self.responses.extend(f"\x1b[{self.row + 1};{self.col + 1}R".encode())
                    elif command == "K":
                        start, end = (0, self.cols) if values[0] == 2 else (0, self.col + 1) if values[0] == 1 else (self.col, self.cols)
                        self.cells[self.row][start:end] = [" "] * (end - start)
                    index += len(match.group(0))
                    continue
                if text[index:].startswith("\x1b]"):
                    end = text.find("\x07", index)
                    if end < 0:
                        self.pending = text[index:]
                        break
                    index = end + 1
                    continue
                if index + 1 >= len(text) or text[index + 1] == "[":
                    self.pending = text[index:]
                    break
                index += 2
                continue
            if char == "\r":
                self.col = 0
            elif char == "\n":
                if self.row == self.rows - 1:
                    self.cells.pop(0)
                    self.cells.append([" "] * self.cols)
                else:
                    self.row += 1
            elif char == "\b":
                self.col = max(0, self.col - 1)
            elif char >= " ":
                self.cells[self.row][self.col] = char
                self.col = min(self.cols - 1, self.col + 1)
            index += 1

    def text(self):
        if self.synchronized_text is not None:
            return self.synchronized_text
        return "\n".join("".join(row) for row in self.cells)

    def resize(self, rows, cols):
        self.synchronized_text = None
        self.rows, self.cols = rows, cols
        self.cells = [[" "] * cols for _ in range(rows)]
        self.row = self.col = 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="bitrouter")
    parser.add_argument("--context", default="administrator")
    parser.add_argument("--expected", choices=("succeeded", "partially_applied"), required=True)
    parser.add_argument("--token-env", default="CLIENT_ADMIN_TOKEN")
    parser.add_argument("--endpoint", default="https://server:8443/control/v1")
    parser.add_argument("--ca", default="/tls/ca.crt")
    parser.add_argument("--read-panels", action="store_true")
    args = parser.parse_args()
    context = ssl.create_default_context(cafile=args.ca)

    def control_get(path, query=None):
        if query:
            path += "?" + urllib.parse.urlencode(query)
        request = urllib.request.Request(
            args.endpoint + path,
            headers={"Authorization": "Bearer " + os.environ[args.token_env]},
        )
        with urllib.request.urlopen(request, context=context, timeout=5) as response:
            return json.load(response)

    def control_post(path, payload):
        request = urllib.request.Request(
            args.endpoint + path,
            data=json.dumps(payload).encode("utf-8"),
            headers={
                "Authorization": "Bearer " + os.environ[args.token_env],
                "Content-Type": "application/json",
            },
            method="POST",
        )
        with urllib.request.urlopen(request, context=context, timeout=5) as response:
            return json.load(response)

    def state():
        return control_get("/state")

    initial = state()
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 60, 180, 0, 0))
    screen = Screen()
    transcript = bytearray()

    def session():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    child = subprocess.Popen([args.binary, "--context", args.context, "code"], stdin=slave, stdout=slave, stderr=slave, close_fds=True, preexec_fn=session, env={**os.environ, "TERM": "xterm-256color"})

    def until(predicate, description, timeout=45):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate(screen.text()):
                return
            if child.poll() is not None:
                raise AssertionError(f"dashboard exited {child.returncode} before {description}:\n{screen.text()}")
            readable, _, _ = select.select([master], [], [], min(0.25, deadline - time.monotonic()))
            if readable:
                data = os.read(master, 65536)
                transcript.extend(data)
                if len(transcript) > 8 * 1024 * 1024:
                    raise AssertionError("PTY output exceeded bound")
                screen.feed(data)
                if screen.responses:
                    os.write(master, screen.responses)
                    screen.responses.clear()
        raise AssertionError(f"timed out waiting for {description}:\n{screen.text()}")

    def resize(rows):
        screen.resize(rows, 180)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 180, 0, 0))

    def exercise_read_panels():
        """Render each remote inspection result and its available read controls."""

        models = control_get("/models")
        model_ids = [model["id"] for model in models["models"]]
        assert model_ids, "fixture did not expose a model for dashboard verification"
        model_count = str(len(model_ids))
        providers = control_get("/providers")["providers"]
        assert providers, "fixture did not expose a provider for dashboard verification"
        provider_id = providers[0]["id"]
        requests = control_get("/requests", {"limit": "5"})["rows"]
        assert requests, "fixture did not persist requests for dashboard verification"
        request_model = requests[0]["model"]
        route = control_post("/route/preview", {"model": "fixture-a"})
        route_requested = route["requested_model"]
        route_effective = route["effective_model"]
        route_providers = [hop["provider"] for hop in route["provider_chain"]]
        assert route_providers, "fixture route preview did not expose a provider chain"
        agents = control_get("/agents")["agents"]
        assert agents, "fixture did not expose an agent catalog for dashboard verification"
        agent_id = {"claude-acp": "claude", "codex-acp": "codex"}.get(
            agents[0]["id"], agents[0]["id"]
        )
        policies = control_get("/policy/status", {"view": "active"})["policies"]
        assert "fixture" in policies and "fixture-detail" in policies, (
            "fixture did not expose the bounded policy-selection data"
        )

        until(
            lambda text: "connected" in text
            and "models" in text
            and re.search(r"\bmodels\s{2,}" + re.escape(model_count) + r"\b", text)
            and provider_id in text,
            "semantic remote home panel",
        )
        home_output = len(transcript)
        os.write(master, b"r")
        until(
            lambda text: len(transcript) > home_output
            and "connected" in text
            and re.search(r"\bmodels\s{2,}" + re.escape(model_count) + r"\b", text),
            "read-only dashboard refresh",
        )

        os.write(master, b"2")
        until(
            lambda text: "Agent catalog" in text
            and agent_id in text
            and "remote catalog is read-only" in text,
            "remote agent catalog",
        )
        os.write(master, b"\r")
        until(
            lambda text: "Remote agent catalogs are read-only" in text,
            "remote agent launch denial",
        )

        os.write(master, b"3")
        until(
            lambda text: "not connected" in text and "Message" in text,
            "remote conversation exclusion",
        )
        os.write(master, b"\t")
        until(
            lambda text: "Native sessions" in text and "No active native session" in text,
            "remote sessions exclusion",
        )

        os.write(master, b"5")
        until(
            lambda text: "Models" in text and model_ids[0] in text,
            "semantic models panel",
        )
        os.write(master, b"6")
        until(
            lambda text: "Recent requests" in text
            and request_model in text
            and provider_id in text,
            "semantic requests panel",
        )

        os.write(master, b"7")
        until(lambda text: "Type a model selector" in text, "route preview input")
        os.write(master, b"fixture-aX")
        until(lambda text: "fixture-aX" in text, "route preview model entry")
        os.write(master, b"\x7f")
        until(
            lambda text: "fixture-a" in text and "fixture-aX" not in text,
            "route preview input backspace",
        )
        os.write(master, b"\r")
        until(
            lambda text: re.search(
                r"\brequested\s{2,}" + re.escape(route_requested) + r"\b", text
            )
            and re.search(
                r"\beffective\s{2,}" + re.escape(route_effective) + r"\b", text
            )
            and re.search(
                r"\bproviders\s{2,}" + re.escape(route_providers[0]) + r"\b", text
            ),
            "semantic route preview",
        )
        os.write(master, b"\x15")
        until(
            lambda text: "Type a model selector" in text,
            "route preview clear",
        )

        os.write(master, b"\t")
        until(
            lambda text: "Providers" in text and provider_id in text and "ACTIVE" in text,
            "semantic providers panel",
        )
        os.write(master, b"9")
        until(
            lambda text: "Telemetry" in text and "daemon" in text and "reachable" in text,
            "semantic telemetry panel",
        )

        os.write(master, b"0")
        until(
            lambda text: "Policy" in text and "active" in text and "fixture-detail" in text,
            "active policy overview",
        )
        os.write(master, b"\x1b[B")
        until(
            lambda text: "> fixture-detail" in text,
            "policy selection",
        )
        selection_after_down = screen.text()
        os.write(master, b"\x1b[A")
        until(
            lambda text: text != selection_after_down,
            "policy previous selection",
        )
        os.write(master, b"\r")
        until(
            lambda text: "Detail: fixture" in text and "Detail: fixture-detail" not in text,
            "policy previous typed detail",
        )
        os.write(master, b"\x1b[B")
        until(
            lambda text: "Detail:" not in text,
            "policy detail reset after next selection",
        )
        os.write(master, b"\r")
        until(
            lambda text: "Detail: fixture-detail" in text
            and "concise" in text
            and "detailed" in text,
            "active policy typed detail",
        )

        # A short PTY makes a real scroll observable instead of only proving
        # that the PageDown key reaches the reducer.
        resize(18)
        until(
            lambda text: "Detail: fixture-detail" in text,
            "resized policy detail",
        )
        def policy_content(text):
            # Refresh timestamps are independent of scrolling. Compare the
            # complete policy body, after the synchronized frame commits.
            lines = text.splitlines()
            start = next(index for index, line in enumerate(lines) if "┌ Policy" in line)
            end = next(index for index in range(start + 1, len(lines)) if "└" in lines[index])
            return lines[start + 1:end]

        before_scroll = policy_content(screen.text())
        os.write(master, b"\x1b[6~")
        until(
            lambda text: policy_content(text) != before_scroll and "Source: active" in text,
            "policy detail scroll",
        )
        os.write(master, b"\x1b[5~")
        until(
            lambda text: policy_content(text) == before_scroll,
            "policy detail scroll restore",
        )

        os.write(master, b"v")
        until(
            lambda text: "Policy" in text and "disk" in text and "fixture-detail" in text,
            "disk policy overview",
        )
        resize(60)
        until(
            lambda text: "Policy" in text and "disk" in text,
            "restored dashboard size",
        )
        os.write(master, b"\r")
        until(
            lambda text: "Detail: fixture-detail" in text and "detailed" in text,
            "disk policy typed detail",
        )

        after_reads = state()
        assert after_reads["generation"] == initial["generation"], (
            "dashboard read panels mutated the router"
        )

    try:
        until(lambda text: "Reload" in text and "remote:" in text, "initial dashboard")
        if args.read_panels:
            exercise_read_panels()
            os.write(master, b"\t")
        else:
            os.write(master, b"\t" * 10)
        until(lambda text: "generation" in text and "instance" in text and "Enter" in text, "reload page")
        before = state()
        assert before["generation"] == initial["generation"], "dashboard startup mutated the router"
        # Explicit refresh must remain read-only. A redraw is observed before
        # sending the reload key, and the request count is checked through state.
        prior_output = len(transcript)
        os.write(master, b"r")
        until(lambda text: len(transcript) > prior_output and "generation" in text and "instance" in text, "read-only refresh")
        assert state()["generation"] == initial["generation"], "dashboard refresh mutated the router"
        os.write(master, b"\r")
        expected = args.expected.replace("_", " ")
        until(lambda text: "request" in text and (args.expected in text or expected in text), "terminal reload outcome")
        final = state()
        assert final["generation"] == initial["generation"] + 1, "explicit reload did not execute exactly once"
        assert final["last_outcome"]["outcome"] == args.expected
        if args.expected == "partially_applied":
            assert final["consistency"] == "mixed"
            until(lambda text: "mixed" in text and "failed" in text, "mixed state and failed participant")
        os.write(master, b"q")
        child.wait(timeout=10)
        assert child.returncode == 0, f"dashboard quit failed: {child.returncode}"
        restored = termios.tcgetattr(slave)
        mask = termios.ICANON | termios.ECHO | termios.ISIG
        assert restored[3] & mask == original[3] & mask, "terminal input modes were not restored"
        shell = subprocess.Popen(["/bin/sh", "-c", "printf 'REMOTE_ADMIN_SHELL_USABLE\\n'"], stdin=slave, stdout=slave, stderr=slave)
        shell.wait(timeout=5)
        deadline = time.monotonic() + 5
        while b"REMOTE_ADMIN_SHELL_USABLE" not in transcript and time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.25)
            if readable:
                transcript.extend(os.read(master, 65536))
        assert b"REMOTE_ADMIN_SHELL_USABLE" in transcript, "subsequent shell output was unusable"
        print(json.dumps({"pty": "passed", "outcome": args.expected, "generation": final["generation"], "read_panels": args.read_panels, "terminal_restored": True, "shell_usable": True}))
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    main()
