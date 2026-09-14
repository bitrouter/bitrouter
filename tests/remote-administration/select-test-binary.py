#!/usr/bin/env python3
"""Copy the single bitrouter lib-test executable named by Cargo JSON output."""

import json
from pathlib import Path
import shutil
import sys


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: select-test-binary.py DESTINATION")

    executables = []
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            message.get("reason") == "compiler-message"
            and message.get("message", {}).get("level") == "error"
        ):
            rendered = message["message"].get("rendered")
            if rendered:
                print(rendered, file=sys.stderr, end="" if rendered.endswith("\n") else "\n")
        target = message.get("target", {})
        if (
            message.get("reason") == "compiler-artifact"
            and target.get("name") == "bitrouter"
            and target.get("kind") == ["lib"]
            and message.get("executable")
        ):
            executables.append(Path(message["executable"]))

    if len(executables) != 1:
        raise SystemExit(
            f"expected one bitrouter lib-test executable, found {len(executables)}"
        )

    destination = Path(sys.argv[1])
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(executables[0], destination)
    destination.chmod(0o755)


if __name__ == "__main__":
    main()
