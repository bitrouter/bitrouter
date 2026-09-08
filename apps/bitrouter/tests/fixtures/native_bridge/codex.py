#!/usr/bin/env python3
"""Deterministic native transport fixture; never calls a model provider.
Protocol: https://developers.openai.com/codex/app-server/
"""
import json
import os
import sys
import threading
import time
import uuid

if '--version' in sys.argv:
    print('codex-cli 0.153.3')
    raise SystemExit(0)

lock = threading.Lock()
threads = {}


def record(direction, value):
    with open(os.environ['BITROUTER_TEST_NATIVE_RECORDS'], 'a', encoding='utf8') as output:
        output.write(json.dumps({'direction': direction, 'body': value}) + '\n')

def send(value):
    with lock:
        record('out', value)
        print(json.dumps(value), flush=True)

def thread(cwd, ephemeral=False):
    ident = str(uuid.uuid4())
    value = {'id': ident, 'preview': '', 'ephemeral': ephemeral,
        'modelProvider': 'openai', 'createdAt': 1, 'updatedAt': 1,
        'status': {'type': 'idle'}, 'path': None, 'cwd': cwd,
        'cliVersion': '0.153.3', 'source': 'appServer', 'turns': []}
    threads[ident] = value
    return value

def complete(ident, turn):
    time.sleep(0.05)
    send({'method': 'turn/started', 'params': {'threadId': ident, 'turn': turn}})
    item = {'type': 'agentMessage', 'id': str(uuid.uuid4()), 'text': 'Fixture complete.', 'phase': 'final_answer'}
    send({'method': 'item/started', 'params': {'threadId': ident, 'turnId': turn['id'], 'item': item}})
    send({'method': 'item/completed', 'params': {'threadId': ident, 'turnId': turn['id'], 'item': item}})
    done = {**turn, 'items': [item], 'status': 'completed'}
    send({'method': 'turn/completed', 'params': {'threadId': ident, 'turn': done}})

for line in sys.stdin:
    request = json.loads(line)
    with lock:
        record('in', request)
    if 'id' not in request:
        continue
    method = request.get('method')
    params = request.get('params') or {}
    print('fixture request: ' + str(method), file=sys.stderr, flush=True)
    later = None
    if method == 'initialize':
        result = {'userAgent': 'codex_cli_rs/0.153.3', 'platformFamily': 'unix', 'platformOs': 'macos'}
    elif method == 'account/read':
        result = {'account': {'type': 'apiKey'}, 'requiresOpenaiAuth': False}
    elif method == 'config/read':
        result = {'config': {}, 'origins': {}, 'layers': []}
    elif method == 'configRequirements/read':
        result = {'requirements': None}
    elif method == 'model/list':
        result = {'data': [{'id': 'fixture-model', 'model': 'fixture-model', 'displayName': 'Fixture',
            'description': 'Fixture', 'hidden': False, 'isDefault': True,
            'defaultReasoningEffort': 'medium', 'supportedReasoningEfforts': [{'reasoningEffort': 'medium', 'description': 'Fixture'}],
            'inputModalities': ['text'], 'supportsPersonality': False}], 'nextCursor': None}
    elif method == 'skills/list':
        result = {'data': [{'cwd': cwd, 'skills': [], 'errors': []} for cwd in params.get('cwds', [])]}
    elif method in ('mcpServerStatus/list', 'plugin/list', 'app/list'):
        result = {'data': [], 'nextCursor': None}
    elif method in ('thread/start', 'thread/fork'):
        t = thread(params.get('cwd', '.'), params.get('ephemeral', False))
        result = {'thread': t, 'model': 'fixture-model', 'modelProvider': 'openai', 'cwd': t['cwd'],
            'approvalPolicy': 'never', 'sandbox': {'type': 'readOnly'}, 'reasoningEffort': 'medium', 'serviceTier': None}
    elif method == 'thread/read':
        result = {'thread': threads[params['threadId']]}
    elif method == 'turn/start':
        turn = {'id': str(uuid.uuid4()), 'items': [], 'status': 'inProgress', 'error': None}
        result = {'turn': turn}
        later = (params['threadId'], turn)
    elif method in ('config/batchWrite', 'config/value/write', 'thread/name/set', 'thread/unsubscribe', 'turn/interrupt'):
        result = {}
    else:
        send({'id': request['id'], 'error': {'code': -32601, 'message': 'unsupported fixture method: ' + str(method)}})
        continue
    send({'id': request['id'], 'result': result})
    if later:
        threading.Thread(target=complete, args=later, daemon=True).start()
