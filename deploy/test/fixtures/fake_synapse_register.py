"""Fake Synapse shared-secret registration (/_synapse/admin/v1/register) for bootstrap.bats.

Usage: fake_synapse_register.py CONFIG PORTFILE LOG [EXISTING_USER]

Hands out nonces, checks the MAC the way Synapse v1.163 does (HMAC-SHA1 keyed with
registration_shared_secret from CONFIG over nonce, username, password and admin/notadmin,
NUL-separated), and answers 400 M_USER_IN_USE for EXISTING_USER. Like Synapse, it creates a
device (a session) for the new account unless the request sets inhibit_login. Each
registration is logged to LOG as "REGISTERED <user> admin=<bool> device=<yes|no>" plus a
"PASSWORD <password>" line. The port it listens on is written to PORTFILE.
"""
import hashlib
import hmac
import json
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

config, portfile, log = sys.argv[1:4]
existing = sys.argv[4] if len(sys.argv) > 4 else ""
with open(config, encoding="utf-8") as f:
    secret = re.search(r"^registration_shared_secret:\s*(\S+)", f.read(), re.M).group(1)
nonces = set()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path != "/_synapse/admin/v1/register":
            return self.answer(404, {"errcode": "M_UNRECOGNIZED"})
        nonce = "nonce%d" % len(nonces)
        nonces.add(nonce)
        self.answer(200, {"nonce": nonce})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if body.get("nonce") not in nonces:
            return self.answer(400, {"errcode": "M_UNKNOWN", "error": "unrecognised nonce"})
        nonces.discard(body["nonce"])
        parts = [body["nonce"], body["username"], body["password"]]
        mac = hmac.new(secret.encode(), b"\x00".join(p.encode() for p in parts)
                       + b"\x00" + (b"admin" if body.get("admin") else b"notadmin"), hashlib.sha1)
        if not hmac.compare_digest(mac.hexdigest(), body.get("mac", "")):
            return self.answer(403, {"errcode": "M_FORBIDDEN", "error": "HMAC incorrect"})
        if body["username"] == existing:
            return self.answer(400, {"errcode": "M_USER_IN_USE", "error": "User ID already taken."})
        device = not body.get("inhibit_login", False)
        with open(log, "a", encoding="utf-8") as f:
            f.write("REGISTERED %s admin=%s device=%s\n" % (body["username"], bool(body.get("admin")),
                                                          "yes" if device else "no"))
            f.write("PASSWORD %s\n" % body["password"])
        result = {"user_id": "@%s:example.com" % body["username"], "home_server": "example.com"}
        if device:
            result.update(access_token="syt_REGISTRATION", device_id="REGDEVICE")
        self.answer(200, result)


server = HTTPServer(("127.0.0.1", 0), Handler)
with open(portfile + ".tmp", "w") as f:
    f.write(str(server.server_port))
os.rename(portfile + ".tmp", portfile)
server.serve_forever()
