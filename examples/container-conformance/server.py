# The conformance container: a small HTTP server on 8080 the Worker drives.
#   /            ok
#   /env         the process's environment, as JSON
#   /fetch?url=  an outbound request from inside, its status and body
#   /write?v=    a marker file in the writable root; /read reads it back
#   /ws?url=&say= a WebSocket from inside: one text frame said, one answered
#   /post?url=&mb= an outbound POST of that many MiB of zeroes, its answer
# SIGTERM exits with 42, so signal() and monitor() can be told apart.
import base64
import http.server
import json
import os
import signal
import socket
import ssl
import struct
import urllib.parse
import urllib.request

CA = "/etc/cloudflare/certs/cloudflare-containers-ca.crt"


def recv_exact(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise ConnectionError("closed")
        data += chunk
    return data


def ws_say(target, text):
    # A plain ws:// client: the upgrade, one masked text frame (under 126
    # bytes), and the first frame back.
    url = urllib.parse.urlparse(target)
    s = socket.create_connection((url.hostname, url.port or 80), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(
        f"GET {url.path or '/'} HTTP/1.1\r\nHost: {url.hostname}\r\nUpgrade: websocket\r\n"
        f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
    )
    head = b""
    while b"\r\n\r\n" not in head:
        head += recv_exact(s, 1)
    status = int(head.split(b" ")[1])
    if status != 101:
        return {"status": status}
    payload = text.encode()
    mask = os.urandom(4)
    s.sendall(bytes([0x81, 0x80 | len(payload)]) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))
    first = recv_exact(s, 2)
    n = first[1] & 0x7F
    if n == 126:
        n = struct.unpack(">H", recv_exact(s, 2))[0]
    return {"status": status, "said": recv_exact(s, n).decode()}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        query = urllib.parse.parse_qs(url.query)
        if url.path == "/":
            body = b"ok"
        elif url.path == "/env":
            body = json.dumps(dict(os.environ)).encode()
        elif url.path == "/fetch":
            target = query["url"][0]
            try:
                context = ssl.create_default_context(cafile=CA) if os.path.exists(CA) else None
                with urllib.request.urlopen(target, timeout=10, context=context) as r:
                    body = json.dumps({"status": r.status, "body": r.read().decode()}).encode()
            except Exception as error:  # the Worker reads the failure
                body = json.dumps({"error": str(error)}).encode()
        elif url.path == "/ws":
            try:
                body = json.dumps(ws_say(query["url"][0], query["say"][0])).encode()
            except Exception as error:  # the Worker reads the failure
                body = json.dumps({"error": str(error)}).encode()
        elif url.path == "/post":
            try:
                data = bytes(int(query["mb"][0]) << 20)
                with urllib.request.urlopen(urllib.request.Request(query["url"][0], data=data, method="POST"), timeout=60) as r:
                    body = json.dumps({"status": r.status, "body": r.read().decode()}).encode()
            except Exception as error:  # the Worker reads the failure
                body = json.dumps({"error": str(error)}).encode()
        elif url.path == "/write":
            with open("/marker", "w") as f:
                f.write(query["v"][0])
            body = b"written"
        elif url.path == "/read":
            body = open("/marker").read().encode() if os.path.exists("/marker") else b"missing"
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


signal.signal(signal.SIGTERM, lambda *args: os._exit(42))
http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
