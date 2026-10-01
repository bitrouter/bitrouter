#!/usr/bin/env python3
"""Exercise native BRO against real inference with isolated state and fixtures."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time

NATURAL = ('Fix the bug in total() so the Python unit tests pass. Inspect the '
           'project first, make the smallest code change, write NOTES.md describing '
           'the fix, and run python3 -m unittest -v. Keep your final answer brief '
           'and state the actual test outcome.')
DIRECTED = ('Inspect this Python project and fix the total() bug so the tests pass. '
            'Start by reading the root directory with limit=2 and follow the returned '
            'offset to view the remaining entries. Use glob to locate Python files, '
            'grep to locate total, and read the implementation. Apply the fix with '
            'edit, create NOTES.md with write describing the change, and run '
            'python3 -m unittest -v with shell. Keep the final answer brief and '
            'state the actual test outcome.')
READONLY = ('Inspect this Python project in read-only mode. Start with a paginated '
            'read of the root directory using limit=2 and follow the next offset; '
            'use glob to discover Python files and grep to locate total(). Read the '
            'implementation and tests and report the bug and their expectations. '
            'Do not modify files or run commands.')


def hashes(directory):
    return {str(p.relative_to(directory)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in directory.rglob('*') if p.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--api-base', default='http://127.0.0.1:4356/v1')
    parser.add_argument('--upstream-model', required=True)
    parser.add_argument('--scenario', choices=('coding', 'readonly', 'natural'), default='coding')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.output_dir.resolve()
    if os.name == 'posix' and len(os.fsencode(root / 'bitrouter.agent.sock')) >= 104:
        parser.error('output-dir is too long for native Unix sockets; use a short /tmp path')
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        parser.error('output-dir must be empty; preserve prior evidence')
    workspace = root / 'project'
    workspace.mkdir()
    (workspace / 'calculator.py').write_text('def total(items):\n    return sum(items) + 1\n')
    (workspace / 'test_calculator.py').write_text(
        'import unittest\nfrom calculator import total\n\n'
        'class TestTotal(unittest.TestCase):\n'
        '    def test_empty(self):\n        self.assertEqual(total([]), 0)\n'
        '    def test_values(self):\n        self.assertEqual(total([2, 3]), 5)\n\n'
        "if __name__ == '__main__':\n    unittest.main()\n")
    (workspace / '.gitignore').write_text('ignored.txt\n__pycache__/\n')
    (workspace / 'ignored.txt').write_text('Visible in directory listings, omitted from search.\n')
    before = hashes(workspace)
    config = root / 'bitrouter.yaml'
    config.write_text(json.dumps({
        'inherit_defaults': False,
        'server': {'listen': '127.0.0.1:0', 'skip_auth': True},
        'database': {'url': f'sqlite://{root}/bitrouter.db?mode=rwc'},
        'providers': {'live-gateway': {
            'api_base': args.api_base,
            'api_protocol': [{'*': 'chat_completions'}],
            'models': [{'id': 'live-test', 'provider_model_id': args.upstream_model}],
        }},
    }, indent=2) + '\n')
    prompt = {'coding': DIRECTED, 'readonly': READONLY, 'natural': NATURAL}[args.scenario]
    (root / 'prompt.txt').write_text(prompt + '\n')
    command = [str(binary), 'task', 'run', prompt, '--model', 'live-test',
               '--workspace', str(workspace), '--config', str(config)]
    command += ['--read-only'] if args.scenario == 'readonly' else ['--check', 'python3 -m unittest -v']
    started = time.monotonic()
    result = None
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=240)
        elapsed = time.monotonic() - started
        (root / 'events.ndjson').write_text(result.stdout)
        (root / 'stderr.txt').write_text(result.stderr)
        rows = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
        terminal = next((row for row in reversed(rows) if row.get('type') == 'terminal'), None)
        payloads = [row['event']['payload'] for row in rows if row.get('type') == 'event']
        calls = [event['name'] for event in payloads if event['kind'] == 'tool_started']
        errors = [event for event in payloads if event['kind'] == 'tool_finished'
                  and (event['output']['type'].startswith('error')
                       or event['output']['type'] in ('execution_denied', 'execution-denied'))]
        records = []
        if (root / 'bitrouter.db').exists() and terminal is not None:
            with sqlite3.connect((root / 'bitrouter.db').as_uri() + '?mode=ro', uri=True) as db:
                records = [json.loads(row[0]) for row in db.execute(
                'SELECT payload FROM bro_execution_records WHERE execution_id=? ORDER BY sequence',
                (terminal['task_id'] if terminal else None,))]
        requests = [record for record in records if record['record'] == 'model_request']
        usages = [record['usage'] for record in records
                  if record['record'] == 'model_response' and record.get('usage') is not None]
        declarations = [[tool['name'] for tool in request['prompt']['tools']] for request in requests]
        summary = {
            'scenario': args.scenario, 'upstream_model': args.upstream_model,
            'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'exit_code': result.returncode, 'elapsed_seconds': round(elapsed, 2),
            'terminal': terminal, 'model_steps': len(requests),
            'declared_tools': declarations[0] if declarations else [],
            'consistent_declarations': all(names == declarations[0] for names in declarations),
            'tool_calls': calls, 'tool_errors': errors,
            'usage_reported_steps': len(usages),
            'first_step_prompt_tokens': usages[0].get('prompt_tokens') if usages else None,
            'unchanged': before == hashes(workspace),
        }
        for field in ('prompt_tokens', 'completion_tokens', 'cache_read_tokens'):
            summary[field] = sum(usage.get(field, 0) for usage in usages) if usages else None
        (root / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(json.dumps(summary, indent=2), flush=True)
        if result.returncode or terminal is None or terminal['status'] != 'completed' or errors:
            raise RuntimeError('live task failed; inspect preserved evidence')
        if args.scenario == 'readonly':
            if not summary['unchanged'] or set(calls) - {'read', 'glob', 'grep'}:
                raise RuntimeError('read-only execution changed files or launched other tools')
        else:
            if terminal['verification'] != 'passed' or (workspace / 'calculator.py').read_text() != 'def total(items):\n    return sum(items)\n':
                raise RuntimeError('independent verification or expected code change failed')
            if not (workspace / 'NOTES.md').is_file():
                raise RuntimeError('requested notes file missing')
            if args.scenario == 'coding' and set(calls) != {'read', 'glob', 'grep', 'write', 'edit', 'shell'}:
                raise RuntimeError('directed task did not exercise all six tools')
    finally:
        stop = subprocess.run([str(binary), 'stop', '--config', str(config)],
                              capture_output=True, text=True, timeout=20)
        (root / 'stop.json').write_text(json.dumps({'exit_code': stop.returncode,
            'stdout': stop.stdout, 'stderr': stop.stderr}, indent=2) + '\n')
        if stop.returncode and result is not None and result.returncode == 0:
            raise RuntimeError('isolated server cleanup failed; inspect stop.json')


if __name__ == '__main__':
    main()
