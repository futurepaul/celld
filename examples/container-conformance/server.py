# The conformance container: a small HTTP server on 8080 the Worker drives.
#   /            ok
#   /env         the process's environment, as JSON
#   /fetch?url=  an outbound request from inside, its status and body
#   /write?v=    a marker file in the writable root; /read reads it back
# SIGTERM exits with 42, so signal() and monitor() can be told apart.
import http.server
import json
import os
import signal
import ssl
import urllib.parse
import urllib.request

CA = "/etc/cloudflare/certs/cloudflare-containers-ca.crt"


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
