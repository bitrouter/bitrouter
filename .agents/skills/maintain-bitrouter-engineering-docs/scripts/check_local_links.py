#!/usr/bin/env python3
"""Check local Markdown links and forbidden docs/skill content symlinks."""

from __future__ import annotations

import re
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[4]
LINK = re.compile(r"]\((?P<target><[^>]+>|[^)\s]+)")
SKIP_PARTS = {".git", "target"}


def skipped(path: Path) -> bool:
    relative = path.relative_to(REPO_ROOT)
    if any(part in SKIP_PARTS for part in relative.parts):
        return True
    return len(relative.parts) >= 2 and relative.parts[:2] == (".claude", "worktrees")


def local_target(raw: str) -> str | None:
    target = raw.removeprefix("<").removesuffix(">")
    if target.startswith(("http://", "https://", "mailto:", "#", "/")):
        return None
    path = target.split("#", 1)[0]
    return re.sub(r":\d+(?:-\d+)?$", "", path) or None


def markdown_failures() -> list[str]:
    failures: list[str] = []
    for document in REPO_ROOT.rglob("*.md"):
        if skipped(document):
            continue
        for line_number, line in enumerate(document.read_text().splitlines(), 1):
            for match in LINK.finditer(line):
                target = local_target(match.group("target"))
                if target is None:
                    continue
                if not (document.parent / target).resolve().exists():
                    relative = document.relative_to(REPO_ROOT)
                    failures.append(f"{relative}:{line_number}: broken link: {target}")
    return failures


def symlink_failures() -> list[str]:
    failures: list[str] = []
    for base in (REPO_ROOT / "docs", REPO_ROOT / ".agents" / "skills"):
        for path in base.rglob("*"):
            if path.is_symlink():
                relative = path.relative_to(REPO_ROOT)
                failures.append(f"{relative}: content symlink is not allowed")
    return failures


def main() -> int:
    failures = markdown_failures() + symlink_failures()
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("local Markdown links and content symlinks are valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
