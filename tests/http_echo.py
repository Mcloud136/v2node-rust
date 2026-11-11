#!/usr/bin/env python3
import hashlib
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BODY = b"x" * 200000 + b"END"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        digest = hashlib.sha256(self.rfile.read(length)).hexdigest().encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(digest)))
        self.end_headers()
        self.wfile.write(digest)

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
