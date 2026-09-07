#!/usr/bin/env python3
"""Deterministic SDK CLI transport, with no model provider calls.
Reference: https://github.com/anthropics/claude-agent-sdk-typescript
"""
import json
import os
import sys
import uuid

if '--version' in sys.argv:
    print('2.1.257 (Claude Code)')
    raise SystemExit(0)
if sys.argv[1:3] == ['auth', 'status']:
    print(json.dumps({'loggedIn': True, 'authMethod': 'api_key', 'apiProvider': 'firstParty'}))
    raise SystemExit(0)


def record(direction, value):
    with open(os.environ['BITROUTER_TEST_NATIVE_RECORDS'], 'a', encoding='utf8') as output:
        output.write(json.dumps({'direction': direction, 'body': value}) + '\n')

def send(value):
    record('out', value)
    print(json.dumps(value), flush=True)

def argument(name, default):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default

session = argument('--session-id', str(uuid.uuid4()))
models = [{'value': 'sonnet', 'displayName': 'Fixture', 'description': 'Deterministic fixture'}]
for line in sys.stdin:
    message = json.loads(line)
    record('in', message)
    print('fixture input: ' + str(message.get('type')) + '/' + str(message.get('request', {}).get('subtype')), file=sys.stderr, flush=True)
    if message.get('type') == 'control_request':
        request = message.get('request', {})
        subtype = request.get('subtype')
        if subtype == 'initialize':
            result = {'commands': [], 'agents': [], 'models': models, 'output_style': 'default', 'available_output_styles': ['default'], 'account': {'apiKeySource': 'environment', 'tokenSource': 'none'}, 'hooks_applied': True}
        elif subtype == 'mcp_status':
            result = {'mcpServers': []}
        elif subtype == 'get_settings':
            result = {'settings': {}}
        elif subtype in ['set_permission_mode', 'set_model', 'apply_flag_settings', 'rename_session', 'mcp_set_servers']:
            result = {}
        else:
            send({'type': 'control_response', 'response': {'subtype': 'error', 'request_id': message['request_id'], 'error': 'Unsupported fixture request: ' + str(subtype)}})
            continue
        send({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': message['request_id'], 'response': result}})
    elif message.get('type') == 'user':
        send({'type': 'system', 'subtype': 'init', 'session_id': session, 'uuid': str(uuid.uuid4()), 'cwd': '.', 'tools': [], 'mcp_servers': [], 'model': 'sonnet', 'permissionMode': 'default', 'slash_commands': [], 'apiKeySource': 'environment', 'claude_code_version': '2.1.257', 'output_style': 'default', 'agents': [], 'skills': [], 'plugins': []})
        send({**message, 'session_id': session, 'parent_tool_use_id': None})
        send({'type': 'assistant', 'session_id': session, 'uuid': str(uuid.uuid4()), 'parent_tool_use_id': None, 'message': {'id': str(uuid.uuid4()), 'type': 'message', 'role': 'assistant', 'model': 'sonnet', 'content': [{'type': 'text', 'text': 'Fixture complete.'}], 'stop_reason': 'end_turn', 'stop_sequence': None, 'usage': {'input_tokens': 1, 'output_tokens': 1}}})
        send({'type': 'result', 'subtype': 'success', 'session_id': session, 'uuid': str(uuid.uuid4()), 'is_error': False, 'duration_ms': 1, 'duration_api_ms': 1, 'num_turns': 1, 'result': 'Fixture complete.', 'stop_reason': 'end_turn', 'total_cost_usd': 0, 'usage': {'input_tokens': 1, 'output_tokens': 1, 'cache_creation_input_tokens': 0, 'cache_read_input_tokens': 0}, 'modelUsage': {}, 'permission_denials': []})
