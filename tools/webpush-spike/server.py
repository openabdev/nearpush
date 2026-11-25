#!/usr/bin/env python3
"""Throwaway Web Push spike server for nearpush (Apple Watch question).

Serves a minimal home-screen web app, stores push subscriptions, and sends
test pushes to web.push.apple.com. Not part of nearpushd.

State directory (outside the repo, mode 0700, files 0600):
  $NEARPUSH_SPIKE_STATE (default ~/.local/share/nearpush-webpush-spike/state)
    vapid_private.pem   VAPID P-256 private key (never printed)
    subscriptions.jsonl one subscription JSON per line
    log.jsonl           request/send log (endpoint host only, no keys)

Usage:
  NEARPUSH_SPIKE_SUBJECT=mailto:<you>   required, used as VAPID "sub"
  server.py serve           # listens on 127.0.0.1:8797
  server.py send STATE      # STATE in your_turn|allow|all_set
"""
import base64
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec
from pywebpush import WebPushException, webpush

HERE = Path(__file__).resolve().parent
STATE = Path(os.environ.get(
    "NEARPUSH_SPIKE_STATE",
    Path.home() / ".local/share/nearpush-webpush-spike/state"))
PORT = 8797
SUBJECT = os.environ.get("NEARPUSH_SPIKE_SUBJECT", "")

STATIC = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/sw.js": ("sw.js", "text/javascript"),
    "/manifest.webmanifest": ("manifest.webmanifest", "application/manifest+json"),
}
ICONS = HERE.parent.parent / "crates/nearpushd/assets"

PAYLOADS = {
    "your_turn": {"title": "nearpush: your turn", "body": "spike: agent is waiting for you"},
    "allow": {"title": "nearpush: allow?", "body": "spike: agent asks for permission"},
    "all_set": {"title": "nearpush: all set", "body": "spike: agent finished"},
}


def log(event, **kw):
    kw.update(t=time.strftime("%Y-%m-%dT%H:%M:%S%z"), event=event)
    line = json.dumps(kw)
    print(line, flush=True)
    with open(STATE / "log.jsonl", "a") as f:
        f.write(line + "\n")


def write_private(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(data)
    os.chmod(path, 0o600)


def vapid_key():
    STATE.mkdir(parents=True, exist_ok=True)
    os.chmod(STATE, 0o700)
    p = STATE / "vapid_private.pem"
    if not p.exists():
        k = ec.generate_private_key(ec.SECP256R1())
        write_private(p, k.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption()))
    k = serialization.load_pem_private_key(p.read_bytes(), None)
    pub = k.public_key().public_bytes(
        serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)
    return p, base64.urlsafe_b64encode(pub).rstrip(b"=").decode()


class H(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="text/plain"):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

    def do_GET(self):
        user = self.headers.get("Tailscale-User-Login")
        log("get", path=self.path, ts_user=user,
            ua=self.headers.get("User-Agent", "")[:160])
        path = urlparse(self.path).path
        if path == "/vapid":
            return self._send(200, json.dumps({"key": vapid_key()[1]}), "application/json")
        if path in STATIC:
            name, ctype = STATIC[path]
            return self._send(200, (HERE / name).read_bytes(), ctype)
        if path in ("/icon-180.png", "/icon-192.png", "/icon-512.png"):
            return self._send(200, (ICONS / path[1:]).read_bytes(), "image/png")
        self._send(404, "not found")

    def do_POST(self):
        user = self.headers.get("Tailscale-User-Login")
        n = int(self.headers.get("Content-Length", "0"))
        if n > 8192:
            return self._send(413, "too large")
        raw = self.rfile.read(n)
        path = urlparse(self.path).path
        if path == "/subscribe":
            try:
                sub = json.loads(raw)
                ep = sub["endpoint"]
                sub["keys"]["p256dh"], sub["keys"]["auth"]
            except Exception as e:
                log("subscribe_bad", err=str(e))
                return self._send(400, "bad subscription")
            fd = os.open(STATE / "subscriptions.jsonl",
                         os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
            with os.fdopen(fd, "a") as f:
                f.write(json.dumps({"ts_user": user, "sub": sub}) + "\n")
            log("subscribed", ts_user=user, endpoint_host=urlparse(ep).hostname)
            return self._send(200, "ok")
        if path == "/client-log":
            log("client", ts_user=user, msg=raw.decode(errors="replace")[:500])
            return self._send(200, "ok")
        self._send(404, "not found")


def send(state):
    key_path, _ = vapid_key()
    payload = dict(PAYLOADS[state], state=state, sent=time.strftime("%H:%M:%S"))
    subs_file = STATE / "subscriptions.jsonl"
    if not subs_file.exists():
        sys.exit("no subscriptions yet")
    # Latest subscription per endpoint.
    subs = {}
    for line in subs_file.read_text().splitlines():
        rec = json.loads(line)
        subs[rec["sub"]["endpoint"]] = rec
    for ep, rec in subs.items():
        host = urlparse(ep).hostname
        t0 = time.time()
        try:
            r = webpush(rec["sub"], json.dumps(payload),
                        vapid_private_key=str(key_path),
                        vapid_claims={"sub": SUBJECT},
                        ttl=300, headers={"Urgency": "high"})
            log("push", state=state, endpoint_host=host, ts_user=rec.get("ts_user"),
                status=r.status_code, ms=int((time.time() - t0) * 1000),
                apns_id=r.headers.get("apns-id"), body=r.text[:200])
        except WebPushException as e:
            resp = e.response
            log("push_error", state=state, endpoint_host=host,
                status=getattr(resp, "status_code", None),
                body=(resp.text[:300] if resp is not None else str(e)))


def main():
    if not SUBJECT:
        sys.exit("NEARPUSH_SPIKE_SUBJECT is not set. Export it as a mailto: URI, "
                 "e.g. NEARPUSH_SPIKE_SUBJECT=mailto:you@example.com")
    cmd = sys.argv[1] if len(sys.argv) > 1 else "serve"
    if cmd == "serve":
        vapid_key()
        log("listen", addr=f"127.0.0.1:{PORT}")
        ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
    elif cmd == "send" and len(sys.argv) == 3 and sys.argv[2] in PAYLOADS:
        send(sys.argv[2])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
