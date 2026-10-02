#!/usr/bin/env python3
"""Submit a reload, receive only its HTTP receipt headers, then lose the body."""

import json
import os
import socket
import ssl
import sys


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: disconnect-submit.py JSON_BODY")

    body = sys.argv[1].encode("utf-8")
    token = os.environ["CLIENT_ADMIN_TOKEN"]
    context = ssl.create_default_context(cafile="/tls/ca.crt")
    raw = socket.create_connection(("server", 8443), timeout=5)
    tls = context.wrap_socket(raw, server_hostname="server")
    request = b"".join(
        [
            b"POST /control/v1/reload HTTP/1.1\r\n",
            b"Host: server:8443\r\n",
            b"Content-Type: application/json\r\n",
            b"Connection: close\r\n",
            f"Authorization: Bearer {token}\r\n".encode("utf-8"),
            f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"),
            body,
        ]
    )
    tls.sendall(request)
    # Consume only the HTTP receipt headers, one byte at a time so a buffered
    # read cannot accidentally consume the operation body. A 202 proves the
    # server admitted the operation; closing now models loss of its report.
    headers = bytearray()
    while b"\r\n\r\n" not in headers:
        byte = tls.recv(1)
        if not byte:
            raise SystemExit("server closed disconnected reload submission before its receipt")
        headers.extend(byte)
        if len(headers) > 16 * 1024:
            raise SystemExit("reload receipt headers exceeded their bound")
    status_line = bytes(headers).split(b"\r\n", 1)[0]
    if b" 202 " not in status_line:
        raise SystemExit(f"reload submission was not admitted: {status_line.decode('ascii', 'replace')}")
    tls.close()


if __name__ == "__main__":
    main()
