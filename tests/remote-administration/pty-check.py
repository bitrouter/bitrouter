#!/usr/bin/env python3
"""Real PTY acceptance for the remote Code operations surface; standard library only.

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
                raise AssertionError(f"Code exited {child.returncode} before {description}:\n{screen.text()}")
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

    def open_command(label, predicate, description):
        os.write(master, b"\x10")
        until(lambda text: "Commands" in text and "Search" in text, "command palette")
        os.write(master, label.encode("utf-8"))
        until(lambda text: label in text, f"{label} command")
        os.write(master, b"\r")
        until(predicate, description)

    def close_to_root():
        os.write(master, b"\x1b")
        until(
            lambda text: "Remote operations" in text and "connected" in text,
            "operations root",
        )

    def exercise_read_inspectors():
        """Open each typed remote read through the no-tabs command palette."""

        models = control_get("/models")["models"]
        providers = control_get("/providers")["providers"]
        requests = control_get("/requests", {"limit": "5"})["rows"]
        agents = control_get("/agents")["agents"]
        assert models and providers and requests and agents

        open_command("Routable models", lambda text: models[0]["id"] in text, "models inspector")
        close_to_root()
        open_command("Host requests", lambda text: requests[0]["model"] in text, "requests inspector")
        close_to_root()
        open_command("Providers", lambda text: providers[0]["id"] in text, "providers inspector")
        close_to_root()
        open_command("Telemetry", lambda text: "OTel" in text, "telemetry inspector")
        close_to_root()
        open_command("Agent catalog", lambda text: agents[0]["id"] in text, "agent catalog inspector")
        close_to_root()
        open_command("Policy status", lambda text: "fixture-detail" in text, "policy status inspector")
        close_to_root()

        open_command("Route preview", lambda text: "Model to preview" in text, "route selector")
        os.write(master, b"fixture-aX")
        until(lambda text: "fixture-aX" in text, "route model entry")
        os.write(master, b"\x7f")
        until(lambda text: "fixture-a" in text and "fixture-aX" not in text, "route model backspace")
        os.write(master, b"\r")
        until(lambda text: "requested" in text and "effective" in text, "route inspector")
        close_to_root()

        open_command("Policy detail", lambda text: "Policy name" in text, "policy selector")
        os.write(master, b"fixture-detail\r")
        until(lambda text: "fixture-detail" in text and "detailed" in text, "policy detail inspector")
        close_to_root()

        open_command("Reload state", lambda text: "generation" in text and "instance" in text, "reload state inspector")
        close_to_root()
        assert state()["generation"] == initial["generation"], "read inspectors mutated the router"

    try:
        until(
            lambda text: "Remote operations" in text and "connected" in text,
            "initial operations inspector",
        )
        if args.read_panels:
            exercise_read_inspectors()
        before = state()
        assert before["generation"] == initial["generation"], "operations startup mutated the router"
        open_command(
            "Reload now",
            lambda text: "request" in text and "reload" in text,
            "explicit reload result",
        )
        expected = args.expected.replace("_", " ")
        until(lambda text: "request" in text and (args.expected in text or expected in text), "terminal reload outcome")
        final = state()
        assert final["generation"] == initial["generation"] + 1, "explicit reload did not execute exactly once"
        assert final["last_outcome"]["outcome"] == args.expected
        if args.expected == "partially_applied":
            assert final["consistency"] == "mixed"
            until(lambda text: "mixed" in text and "failed" in text, "mixed state and failed participant")
        os.write(master, b"\x1b\x1b")
        child.wait(timeout=10)
        assert child.returncode == 0, f"operations surface quit failed: {child.returncode}"
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
