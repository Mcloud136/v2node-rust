#!/usr/bin/env python3
import argparse
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


class Handler(BaseHTTPRequestHandler):
    log_path = ""

    def _log(self, line):
        with open(self.log_path, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")

    def _json(self, payload, status=200):
        body = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path.rstrip("/") == "/client/v4/zones":
            name = parse_qs(parsed.query).get("name", [""])[0]
            self._log(
                f"GET zones name={name} auth={bool(self.headers.get('Authorization'))}"
            )
            self._json({"success": True, "result": [{"id": "zone-123", "name": name}]})
            return
        self._json({"success": False, "errors": [{"message": "not found"}]}, 404)

    def do_POST(self):
        parsed = urlparse(self.path)
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length).decode("utf-8", "replace")
        self._log(f"POST {parsed.path} body={body}")
        if parsed.path.rstrip("/") == "/client/v4/zones/zone-123/dns_records":
            self._json({"success": True, "result": {"id": "record-456"}})
            return
        self._json({"success": False}, 404)

    def do_DELETE(self):
        parsed = urlparse(self.path)
        self._log(f"DELETE {parsed.path}")
        if parsed.path.startswith("/client/v4/zones/zone-123/dns_records/"):
            self._json({"success": True, "result": {"id": "record-456"}})
            return
        self._json({"success": False}, 404)

    def log_message(self, *_args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=18084)
    parser.add_argument("--dir", required=True)
    args = parser.parse_args()
    Handler.log_path = os.path.join(args.dir, "cloudflare.log")
    open(Handler.log_path, "a", encoding="utf-8").close()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
