#!/usr/bin/env python3
"""Serve whep.html and pass its WHEP calls on to oxrtc, from one origin.

oxrtc's API answers no CORS header and asks every call for its credential, so
a web page cannot call it from another origin. This script is the smallest
thing that makes one origin of the two: it serves whep.html (beside it) and
forwards three calls, and only those, to the API with the credential:

    POST   /api/webrtc?src=<stream>   the offer; the answer comes back
    PATCH  /api/webrtc/<id>           candidates trickled later
    DELETE /api/webrtc/<id>           the end of the session

Usage (Python 3.8 or newer, standard library only):

    OXRTC_PASSWORD=change-me python3 serve.py --api http://127.0.0.1:1984 --user admin

then open http://127.0.0.1:8080/?src=<stream name>. Without OXRTC_PASSWORD
the password is asked for at start.

Whoever can reach this script can watch every stream of the server: it adds
no access control of its own. It listens on loopback unless --listen says
otherwise; in front of anything wider, use a reverse proxy that authenticates
its users. Media does not pass through here: the browser reaches oxrtc's
WebRTC port (webrtc.listen) by itself.
"""
import argparse
import base64
import getpass
import http.client
import os
import re
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PAGE = Path(__file__).resolve().with_name("whep.html")
MAX_BODY = 64 * 1024  # oxrtc's own cap on a request body
SESSION = re.compile(r"^/api/webrtc/[0-9A-Za-z_-]{1,64}$")
PASSED_BACK = ("Content-Type", "Location", "Accept-Patch")


class Handler(BaseHTTPRequestHandler):
    api = None  # urllib.parse.SplitResult of --api
    authorization = ""

    def do_GET(self):
        if urllib.parse.urlsplit(self.path).path not in ("/", "/whep.html"):
            return self.reply(404, b"not found")
        self.reply(200, PAGE.read_bytes(), {"Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store"})

    def do_POST(self):
        url = urllib.parse.urlsplit(self.path)
        if url.path != "/api/webrtc" or "src" not in urllib.parse.parse_qs(url.query):
            return self.reply(404, b"not found")
        self.forward()

    def do_PATCH(self):
        self.forward() if SESSION.match(self.path) else self.reply(404, b"not found")

    def do_DELETE(self):
        self.forward() if SESSION.match(self.path) else self.reply(404, b"not found")

    def forward(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length > MAX_BODY:
            return self.reply(413, b"body too large")
        body = self.rfile.read(length)
        headers = {"Authorization": self.authorization, "Content-Length": str(len(body))}
        for name in ("Content-Type", "User-Agent"):
            if self.headers.get(name):
                headers[name] = self.headers[name]
        secure = self.api.scheme == "https"
        connection = (http.client.HTTPSConnection if secure else http.client.HTTPConnection)(self.api.netloc, timeout=65)
        try:
            connection.request(self.command, self.path, body=body, headers=headers)
            answer = connection.getresponse()
            passed = {name: answer.getheader(name) for name in PASSED_BACK if answer.getheader(name)}
            self.reply(answer.status, answer.read(), passed)
        except OSError as error:
            self.reply(502, f"oxrtc is not reachable: {error}".encode())
        finally:
            connection.close()

    def reply(self, status, body, headers=None):
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    parser = argparse.ArgumentParser(description="Serve whep.html and forward its WHEP calls to oxrtc.")
    parser.add_argument("--api", default="http://127.0.0.1:1984", help="oxrtc's API (api.listen)")
    parser.add_argument("--user", default="admin", help="api.username")
    parser.add_argument("--listen", default="127.0.0.1:8080", help="where this script listens")
    args = parser.parse_args()
    password = os.environ.get("OXRTC_PASSWORD") or getpass.getpass(f"Password of {args.user} at {args.api}: ")
    Handler.api = urllib.parse.urlsplit(args.api)
    Handler.authorization = "Basic " + base64.b64encode(f"{args.user}:{password}".encode()).decode()
    host, _, port = args.listen.rpartition(":")
    server = ThreadingHTTPServer((host, int(port)), Handler)
    print(f"open http://{args.listen}/?src=<stream name>", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
