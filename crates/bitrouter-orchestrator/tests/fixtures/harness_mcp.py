import json
import os
import sys

with open("mcp.pid", "w") as pid_file:
    pid_file.write(str(os.getpid()))
if "descendant" in sys.argv:
    import subprocess
    descendant = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    with open("mcp-child.pid", "w") as pid_file:
        pid_file.write(str(descendant.pid))
if "hang" in sys.argv:
    import time
    time.sleep(60)

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request.get('method')
    if method == 'initialize':
        result = {'protocolVersion': '2025-11-25', 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'stdio-fixture', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'echo', 'inputSchema': {'type': 'object'}}]}
    elif method == 'tools/call':
        result = {'content': [{'type': 'text', 'text': os.getcwd()}]}
    else:
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
    if method == 'tools/call' and 'notify' in sys.argv:
        print(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/tools/list_changed'}), flush=True)
