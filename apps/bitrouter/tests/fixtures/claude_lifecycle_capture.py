"""Capture Claude Code 2.1.220 history through a real BitRouter proxy.

Uses an isolated profile, scratch workspace and deterministic local Anthropic
SSE. Native sessions, command events and transcripts come from the real CLI.
The proxy's creation-origin records and an ACP controller are not simulated.

https://code.claude.com/docs/en/cli-reference
https://code.claude.com/docs/en/agent-sdk/sessions
https://platform.claude.com/docs/en/build-with-claude/streaming
"""
import argparse
import hashlib
import json
import os
import queue
import subprocess
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--claude', type=Path, required=True)
parser.add_argument('--bitrouter', type=Path, required=True)
parser.add_argument('--sdk-module', type=Path,
                    help='Also capture standalone SDK 0.3.257 forks and native resumes')
parser.add_argument('--node', default='node')
args = parser.parse_args()
native = args.claude.resolve(strict=True)
router = args.bitrouter.resolve(strict=True)
sdk_module = args.sdk_module.resolve(strict=True) if args.sdk_module else None
if sdk_module:
    package = json.loads((sdk_module.parent / 'package.json').read_text())
    if (package.get('name'), package.get('version')) != ('@anthropic-ai/claude-agent-sdk', '0.3.257'):
        raise RuntimeError('Standalone fork capture requires the published SDK 0.3.257')
root = Path(tempfile.mkdtemp(prefix='bitrouter-claude-lifecycle-conformance-')).resolve()
profile, workspace, spool = (root / name for name in ('profile', 'workspace', 'proxy'))
for path in (profile, workspace, spool, root / 'snapshots'):
    path.mkdir()
namespace = 'sha256:' + hashlib.sha256(json.dumps(
    ['claude_code', str(profile / 'projects')], separators=(',', ':')
).encode()).hexdigest()
env = {k: v for k, v in os.environ.items() if k in ('PATH', 'TMPDIR', 'SystemRoot')}
env.update(CLAUDE_CONFIG_DIR=str(profile), ANTHROPIC_API_KEY='fixture-local-key',
           DISABLE_AUTOUPDATER='1', CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1',
           NO_PROXY='127.0.0.1,localhost,::1')
version = subprocess.run([str(native), '--version'], cwd=workspace, env=env,
                         capture_output=True, text=True, check=True, timeout=15).stdout.strip()
if version != '2.1.220 (Claude Code)':
    raise RuntimeError('This capture requires Claude Code 2.1.220, got ' + version)
requests = []
request_lock = threading.Lock()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        size = int(self.headers.get('content-length', '0'))
        if not 0 < size <= 1024 * 1024:
            self.send_error(413)
            return
        payload = json.loads(self.rfile.read(size))
        with request_lock:
            requests.append({'path': self.path, 'body': payload})
            number = len(requests)
        if 'count_tokens' in self.path:
            body = json.dumps({'input_tokens': 5}).encode()
            content_type = 'application/json'
        else:
            message = {'id': f'msg_fixture_{number}', 'type': 'message', 'role': 'assistant',
                       'model': payload['model'], 'content': [], 'stop_reason': None,
                       'stop_sequence': None, 'usage': {'input_tokens': 5, 'output_tokens': 0}}
            events = [
                {'type': 'message_start', 'message': message},
                {'type': 'content_block_start', 'index': 0,
                 'content_block': {'type': 'text', 'text': ''}},
                {'type': 'content_block_delta', 'index': 0,
                 'delta': {'type': 'text_delta', 'text': f'Fixture completion {number}.'}},
                {'type': 'content_block_stop', 'index': 0},
                {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn', 'stop_sequence': None},
                 'usage': {'output_tokens': 3}},
                {'type': 'message_stop'},
            ]
            body = ''.join('event: ' + e['type'] + '\ndata: ' + json.dumps(e) + '\n\n'
                           for e in events).encode()
            content_type = 'text/event-stream'
        self.send_response(200)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


server = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
threading.Thread(target=server.serve_forever, daemon=True).start()
env.update(ANTHROPIC_BASE_URL=f'http://127.0.0.1:{server.server_port}',
           BITROUTER_CLAUDE_EVIDENCE_SPOOL=str(spool),
           BITROUTER_CLAUDE_EVIDENCE_NAMESPACE=namespace,
           BITROUTER_CLAUDE_EVIDENCE_UPSTREAM=str(native))
proxy = root / 'bitrouter-claude-proxy'
proxy.symlink_to(router)
base = [str(proxy), '--bare', '--print', '--verbose', '--input-format', 'stream-json',
        '--output-format', 'stream-json', '--replay-user-messages', '--setting-sources', '',
        '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--tools', '',
        '--permission-mode', 'dontAsk', '--model', 'claude-opus-4-6']
operations, processes = [], []


def transcript(session):
    paths = list((profile / 'projects').rglob(session + '.jsonl'))
    if len(paths) != 1:
        raise RuntimeError('Expected one transcript for ' + session)
    return paths[0]


class Peer:
    def __init__(self, session, options):
        self.session = session
        self.operation_start = len(operations)
        self.inbox = queue.Queue()
        self.before = set(spool.glob('cli-*.jsonl'))
        self.err = (root / f'process-{len(processes)}-stderr.txt').open('w')
        self.process = subprocess.Popen(base + options, cwd=workspace, env=env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.err, text=True)

        def read():
            try:
                for line in self.process.stdout:
                    self.inbox.put(json.loads(line))
            except (OSError, ValueError) as error:
                self.inbox.put({'transport_error': str(error)})
            finally:
                self.inbox.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()

    def send(self, label, compact=False, prompt=None):
        command = str(uuid.uuid4())
        message = {'type': 'user', 'session_id': self.session, 'uuid': command,
                   'parent_tool_use_id': None, 'message': {'role': 'user', 'content': [
                       {'type': 'text', 'text': '/compact' if compact else (prompt or 'Run the fixture task.')}]}}
        self.process.stdin.write(json.dumps(message) + '\n')
        self.process.stdin.flush()
        deadline, observed = time.monotonic() + 45, []
        try:
            while True:
                event = self.inbox.get(timeout=max(.01, deadline - time.monotonic()))
                if event is None or 'transport_error' in event:
                    raise RuntimeError('Native transport closed during ' + label)
                observed.append(event)
                if len(observed) > 1024:
                    raise RuntimeError('Native event limit')
                if event.get('type') == 'command_lifecycle' and event.get('command_uuid') == command:
                    if event.get('state') == 'completed':
                        break
        finally:
            (root / (label + '-stdout.jsonl')).write_text(
                ''.join(json.dumps(event) + '\n' for event in observed))
        results = [e for e in observed if e.get('type') == 'result']
        if len(results) != 1 or results[0].get('is_error') is not False:
            raise RuntimeError('Missing native success result for ' + label)
        if compact and not any(e.get('subtype') == 'compact_boundary' for e in observed):
            raise RuntimeError('Native compaction did not occur')
        operations.append({'label': label, 'session': self.session, 'command': command,
                           'kind': 'compact' if compact else 'prompt'})
        print(json.dumps(operations[-1]), flush=True)

    def close(self):
        try:
            self.process.stdin.close()
        except BrokenPipeError:
            pass
        try:
            self.process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=6)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        self.err.close()
        if self.process.returncode or self.reader.is_alive():
            raise RuntimeError('Native transport did not exit cleanly')
        created = set(spool.glob('cli-*.jsonl')) - self.before
        if len(created) != 1:
            raise RuntimeError('Expected one native process spool')
        path = created.pop()
        rows = list(map(json.loads, path.read_text().splitlines()))
        if rows[0].get('scope_valid') is not True:
            raise RuntimeError('Proxy namespace mismatch')
        process_id = rows[0]['process_id']
        for operation in operations[self.operation_start:]:
            operation['process_id'] = process_id
        processes.append({'process_id': process_id, 'session': self.session,
                          'spool': str(path.relative_to(root))})


def sdk_observe(session, label, fork_options=None):
    # The standalone export differs from query({forkSession:true}) and the
    # CLI --fork-session switch. Invoke the published implementation itself.
    # https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257
    script = '''
const {pathToFileURL} = await import('node:url');
const sdk = await import(pathToFileURL(process.argv[1]));
const options = JSON.parse(process.argv[4]);
const fork = options === null ? null : await sdk.forkSession(process.argv[2], {
  dir: process.argv[3], ...options
});
const sessionId = fork?.sessionId ?? process.argv[2];
const messages = await sdk.getSessionMessages(sessionId, {
  dir: process.argv[3], includeSystemMessages: true
});
process.stdout.write(JSON.stringify({sessionId, fork, messages}));
'''
    output = subprocess.run([args.node, '--input-type=module', '-e', script,
                             str(sdk_module), session, str(workspace), json.dumps(fork_options)],
                            cwd=workspace, env=env, capture_output=True, text=True, timeout=30)
    (root / (label + '-sdk-stderr.txt')).write_text(output.stderr)
    output.check_returncode()
    result = json.loads(output.stdout)
    path = root / (label + '-sdk.json')
    path.write_text(json.dumps(result, indent=2))
    return result, str(path.relative_to(root))


def sdk_snapshot(session, label):
    path = root / 'snapshots' / (label + '.jsonl')
    path.write_bytes(transcript(session).read_bytes())
    _, reader = sdk_observe(session, label)
    return {'transcript': str(path.relative_to(root)), 'reader': reader}


def sdk_case(source, label, up_to=None, compact=False):
    global peer
    parent_bytes = transcript(source).read_bytes()
    parent_path = root / 'snapshots' / (label + '-parent.jsonl')
    parent_path.write_bytes(parent_bytes)
    options = {'title': 'Fixture ' + label}
    if up_to:
        options['upToMessageId'] = up_to
    forked, fork_result = sdk_observe(source, label + '-fork', options)
    session = forked['sessionId']
    before = sdk_snapshot(session, label + '-before')
    with request_lock:
        start = len(requests)
    peer = Peer(session, ['--resume', session])
    peer.send(label + '-resume', prompt='Continue ' + label + '.')
    peer.close()
    peer = None
    with request_lock:
        indices = [i for i in range(start, len(requests))
                   if '/messages' in requests[i]['path'] and 'count_tokens' not in requests[i]['path']]
    if len(indices) != 1:
        raise RuntimeError('Expected one model request for SDK fork resume')
    after = sdk_snapshot(session, label + '-after')
    local_compact = None
    if compact:
        peer = Peer(session, ['--resume', session])
        peer.send(label + '-compact', compact=True)
        peer.send(label + '-after-compact', prompt='Continue after the SDK fork compaction.')
        peer.close()
        peer = None
        local_compact = sdk_snapshot(session, label + '-compacted')
    if transcript(source).read_bytes() != parent_bytes:
        raise RuntimeError('SDK fork or native child continuation changed its parent transcript')
    return {'label': label, 'parent': source, 'session': session, 'up_to': up_to,
            'parent_transcript': str(parent_path.relative_to(root)), 'fork_result': fork_result,
            'before': before, 'after': after, 'compacted': local_compact,
            'request_index': indices[0]}


parent, fork = str(uuid.uuid4()), str(uuid.uuid4())
peer = None
try:
    peer = Peer(parent, ['--session-id', parent])
    peer.send('initial')
    peer.send('second')
    peer.close()
    (root / 'snapshots/parent-before-fork.jsonl').write_bytes(transcript(parent).read_bytes())
    peer = Peer(fork, ['--resume', parent, '--fork-session', '--session-id', fork])
    peer.send('fork')
    peer.close()
    frozen_fork = transcript(fork).read_bytes()
    (root / 'snapshots/fork-before-parent-continuation.jsonl').write_bytes(frozen_fork)
    peer = Peer(parent, ['--resume', parent])
    peer.send('parent-later')
    peer.send('compact-first', compact=True)
    peer.send('after-compact-first')
    peer.send('compact-second', compact=True)
    peer.send('after-compact-second')
    peer.close()
    peer = Peer(parent, ['--resume', parent])
    peer.send('resume-compacted')
    peer.close()
    peer = None
    if transcript(fork).read_bytes() != frozen_fork:
        raise RuntimeError('Parent continuation changed the independent fork')
    (root / 'capture.json').write_text(json.dumps({
        'schema': 'claude-native-capture/1', 'version': version, 'namespace': namespace,
        'parent': parent, 'fork': fork, 'operations': operations, 'processes': processes,
        'transcripts': {s: str(transcript(s).relative_to(root)) for s in (parent, fork)},
    }, indent=2))
    if sdk_module:
        first = sdk_case(parent, 'sdk-full', compact=True)
        nested = sdk_case(first['session'], 'sdk-nested')
        parent_rows = list(map(json.loads, transcript(parent).read_text().splitlines()))
        compact_index = next(i for i, row in enumerate(parent_rows)
                             if row.get('subtype') == 'compact_boundary')
        cut = next(row['uuid'] for row in parent_rows[compact_index + 1:]
                   if row.get('type') == 'assistant')
        bounded = sdk_case(parent, 'sdk-bounded', up_to=cut)
        (root / 'sdk-forks.json').write_text(json.dumps({
            'schema': 'claude-sdk-fork-capture/1', 'version': version, 'sdk_version': '0.3.257',
            'namespace': namespace, 'cases': [first, nested, bounded],
        }, indent=2))
finally:
    try:
        if peer is not None and peer.process.poll() is None:
            peer.close()
    finally:
        server.shutdown()
        server.server_close()
        (root / 'requests.json').write_text(json.dumps(requests, indent=2))
        print('CAPTURE', root, flush=True)
