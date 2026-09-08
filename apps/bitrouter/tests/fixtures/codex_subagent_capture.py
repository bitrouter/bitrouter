"""Capture Codex 0.153.4 subagent execution through a real BitRouter proxy.

Uses only an isolated profile, a scratch workspace and local deterministic SSE.
Official producer contracts:
https://github.com/openai/codex/tree/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/core/src/tools/handlers/multi_agents_v2
https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/models.rs
"""
import argparse
import json
import os
import queue
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--codex', type=Path, required=True, help='Codex CLI 0.153.4 binary')
parser.add_argument('--bitrouter', type=Path, required=True, help='Built BitRouter binary')
args = parser.parse_args()
binary = args.codex.resolve(strict=True)
router = args.bitrouter.resolve(strict=True)
version = subprocess.run([str(binary), '--version'], capture_output=True, text=True, check=True).stdout.strip()
if version != 'codex-cli 0.153.4':
    raise RuntimeError('This capture requires codex-cli 0.153.4, got ' + version)
root = Path(tempfile.mkdtemp(prefix='bitrouter-codex-subagent-conformance-'))
profile = root / 'profile'
profile.mkdir()
workspace = root / 'workspace'
workspace.mkdir()
requests = []
request_lock = threading.Lock()
parent_requests = {}

class Provider(BaseHTTPRequestHandler):

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with request_lock:
            number = len(requests) + 1
            requests.append({'path': self.path, 'metadata': body.get('client_metadata'), 'headers': {k: v for (k, v) in self.headers.items() if k.lower() in ('thread-id', 'x-codex-turn-metadata')}, 'tools': body.get('tools')})
        if self.path.endswith('/compact'):
            response = {'id': f'compact-{number}', 'object': 'response.compaction', 'output': [{'type': 'compaction', 'id': f'compact-item-{number}', 'encrypted_content': 'fixture-opaque-context'}], 'usage': {'input_tokens': 10, 'output_tokens': 5, 'total_tokens': 15}}
            data = json.dumps(response).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        item = {'type': 'message', 'id': f'fixture-message-{number}', 'role': 'assistant', 'status': 'completed', 'content': [{'type': 'output_text', 'text': f'Fixture completion {number}.'}]}
        meta = json.loads(body.get('client_metadata', {}).get('x-codex-turn-metadata', '{}'))
        if meta.get('agent_name') == '/root' or number == 1:
            turn_id = meta.get('turn_id', 'first')
            n = parent_requests.get(turn_id, 0)
            parent_requests[turn_id] = n + 1
            if n == 0:
                item = {'type': 'function_call', 'namespace': 'collaboration', 'id': f'fc-{number}', 'call_id': f'call-{number}', 'name': 'spawn_agent' if number == 1 else 'followup_task', 'arguments': json.dumps({'task_name': 'worker', 'message': 'Child first', 'fork_turns': 'all'} if number == 1 else {'target': 'worker', 'message': 'Child followup'})}
        response = {'id': f'fixture-response-{number}', 'status': 'completed', 'output': [item], 'usage': {'input_tokens': 10, 'output_tokens': 5, 'total_tokens': 15}}
        events = [{'type': 'response.created', 'response': {'id': response['id'], 'status': 'in_progress', 'output': []}}, {'type': 'response.output_item.done', 'output_index': 0, 'item': item}, {'type': 'response.completed', 'response': response}]
        data = ''.join(('data: ' + json.dumps(e) + '\n\n' for e in events)).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
server = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
threading.Thread(target=server.serve_forever, daemon=True).start()
(profile / 'config.toml').write_text(f'model = "gpt-5.4"\nmodel_provider = "fixture"\n[features]\nmulti_agent = true\nmulti_agent_v2 = true\n[model_providers.fixture]\nname = "Fixture"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\nwire_api = "responses"\nrequires_openai_auth = false\nsupports_websockets = false\n')
native_env = {k: v for (k, v) in os.environ.items() if k in ('PATH', 'TMPDIR', 'SystemRoot')}
native_env.update(CODEX_HOME=str(profile), NO_PROXY='127.0.0.1,localhost,::1', BITROUTER_CODEX_EVIDENCE_SPOOL=str(root / 'proxy'), BITROUTER_CODEX_EVIDENCE_UPSTREAM=str(binary))
events = []
replies = {}
inbox = queue.Queue()
counter = 0
err = (root / 'stderr.log').open('w')
process = subprocess.Popen([str(router), 'app-server'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True, env=native_env, cwd=workspace)

def read():
    for line in process.stdout:
        try:
            inbox.put(json.loads(line))
        except ValueError:
            inbox.put({'non_json': line})
    inbox.put({'closed': True})
threading.Thread(target=read, daemon=True).start()

def receive(deadline):
    event = inbox.get(timeout=max(0.01, deadline - time.monotonic()))
    events.append(event)
    if 'id' in event:
        replies[event['id']] = event
    if event.get('closed'):
        raise RuntimeError('native process closed')
    return event

def call(method, params):
    global counter
    counter += 1
    rid = counter
    process.stdin.write(json.dumps({'id': rid, 'method': method, 'params': params}) + '\n')
    process.stdin.flush()
    deadline = time.monotonic() + 30
    while rid not in replies:
        receive(deadline)
    response = replies.pop(rid)
    if 'error' in response:
        raise RuntimeError(method + ': ' + json.dumps(response['error']))
    return response['result']

def turn(thread, label):
    result = call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': label, 'text_elements': []}]})
    tid = result['turn']['id']
    deadline = time.monotonic() + 30

    def done(e):
        return e.get('method') == 'turn/completed' and e.get('params', {}).get('turn', {}).get('id') == tid
    while not any((done(e) for e in events)):
        receive(deadline)
    end = next((e for e in events if done(e)))
    if end['params']['turn']['status'] != 'completed':
        raise RuntimeError('turn did not complete')
    print(json.dumps({'label': label, 'thread': thread, 'turn': tid}), flush=True)
    return tid
try:
    call('initialize', {'clientInfo': {'name': 'bitrouter_fixture', 'version': '1'}, 'capabilities': {'experimentalApi': True}})
    process.stdin.write(json.dumps({'method': 'initialized', 'params': {}}) + '\n')
    process.stdin.flush()
    parent = call('thread/start', {'cwd': str(workspace), 'model': 'gpt-5.4', 'modelProvider': 'fixture', 'approvalPolicy': 'never', 'sandbox': 'read-only', 'historyMode': 'paginated', 'experimentalRawEvents': True})['thread']['id']
    first = turn(parent, 'Spawn a fixture child')
    deadline = time.monotonic() + 30

    def child_finished():
        for p in (profile / 'sessions').rglob('*.jsonl'):
            rows = [json.loads(s) for s in p.read_text().splitlines()]
            if rows[0]['payload'].get('parent_thread_id') == parent and any((r.get('type') == 'event_msg' and r['payload'].get('type') == 'task_complete' for r in rows)):
                return True
        return False
    while not child_finished():
        if time.monotonic() > deadline:
            raise RuntimeError('child did not complete')
        try:
            receive(min(deadline, time.monotonic() + 0.2))
        except queue.Empty:
            pass
    before = sum((1 for p in (profile / 'sessions').rglob('*.jsonl') for s in p.read_text().splitlines() if json.loads(s).get('type') == 'event_msg' and json.loads(s)['payload'].get('type') == 'task_complete'))
    turn(parent, 'Resume the fixture child')
    deadline = time.monotonic() + 30
    while True:
        count = sum((1 for p in (profile / 'sessions').rglob('*.jsonl') for s in p.read_text().splitlines() if json.loads(s).get('type') == 'event_msg' and json.loads(s)['payload'].get('type') == 'task_complete'))
        if count >= before + 2:
            break
        if time.monotonic() > deadline:
            raise RuntimeError('resumed child did not complete')
        try:
            receive(min(deadline, time.monotonic() + 0.2))
        except queue.Empty:
            pass
    print('PROBE_COMPLETE', root, flush=True)
finally:
    process.stdin.close()
    try:
        process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    server.shutdown()
    server.server_close()
    err.close()
    (root / 'events.json').write_text(json.dumps(events, indent=2))
    (root / 'requests.json').write_text(json.dumps(requests, indent=2))
    for p in (profile / 'sessions').rglob('*.jsonl'):
        rows = [json.loads(s) for s in p.read_text().splitlines()]
        meta = next((r['payload'] for r in rows if r.get('type') == 'session_meta'), {})
        summary = {k: meta.get(k) for k in ['id', 'session_id', 'history_mode', 'history_base', 'forked_from_id', 'forked_from_ordinal_exclusive', 'subagent_history_start_ordinal']}
        print(json.dumps({'file': p.name, 'metadata': summary, 'ordinals': [r.get('ordinal') for r in rows], 'types': [r.get('type') for r in rows]}), flush=True)
    print('CAPTURE', root, flush=True)
