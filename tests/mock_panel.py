#!/usr/bin/env python3
import argparse
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse


class Handler(BaseHTTPRequestHandler):
    state_path = ""
    request_log = ""
    report_log = ""
    directory = ""

    def _state(self):
        with open(self.state_path, encoding="utf-8") as handle:
            return json.load(handle)

    def _failing(self, name):
        return os.path.exists(os.path.join(self.directory, name))

    def _log_request(self, status, path):
        with open(self.request_log, "a", encoding="utf-8") as handle:
            handle.write(f"{self.command} {path} {status}\n")

    def _send_json(self, payload, etag=None, status=200):
        body = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if etag:
            self.send_header("ETag", etag)
        self.end_headers()
        self.wfile.write(body)

    def _send_304(self):
        self.send_response(304)
        self.end_headers()

    def do_GET(self):
        path = urlparse(self.path).path
        state = self._state()
        version = str(state.get("etag", "1"))
        if path == "/api/v2/server/config":
            etag = f'"node-{version}"'
            if self.headers.get("If-None-Match") == etag:
                self._send_304()
                self._log_request(304, path)
                return
            self._send_json(state["node"], etag=etag)
            self._log_request(200, path)
            return
        if path == "/api/v1/server/UniProxy/user":
            etag = f'"user-{version}"'
            if self.headers.get("If-None-Match") == etag:
                self._send_304()
                self._log_request(304, path)
                return
            self._send_json(state["users"], etag=etag)
            self._log_request(200, path)
            return
        if path == "/api/v1/server/UniProxy/alivelist":
            self._send_json(state.get("alive", {"alive": {}}))
            self._log_request(200, path)
            return
        self._send_json({"error": "not found"}, status=404)
        self._log_request(404, path)

    def do_POST(self):
        path = urlparse(self.path).path
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        if path.endswith("/alive") and self._failing("fail_alive"):
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()
            self._log_request(500, path)
            return
        with open(self.report_log, "ab") as handle:
            handle.write(path.encode() + b" " + body + b"\n")
        if path.endswith("/push") and self._failing("fail_push"):
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()
            self._log_request(500, path)
            return
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")
        self._log_request(200, path)

    def log_message(self, *_args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--state", required=True)
    parser.add_argument("--port", type=int, default=18080)
    parser.add_argument("--dir", required=True)
    args = parser.parse_args()
    Handler.state_path = args.state
    Handler.request_log = os.path.join(args.dir, "requests.log")
    Handler.report_log = os.path.join(args.dir, "reports.log")
    Handler.directory = args.dir
    for path in (Handler.request_log, Handler.report_log):
        open(path, "a", encoding="utf-8").close()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
