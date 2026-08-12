#!/usr/bin/env python3
"""Minimal fake Telegram Bot API for counting OpenFang getUpdates pollers.

Implements just enough of the Bot API for `TelegramAdapter::start()` to
succeed (getMe / deleteWebhook / setMyCommands) plus `getUpdates` with the
real single-reader semantics:

  * only one getUpdates may be in flight per bot token;
  * a newly arriving getUpdates takes the queue over ("newcomer wins");
  * the request it displaced is answered with HTTP 409 Conflict after a
    short server-side delay (CONFLICT_DELAY), exactly like
    "Conflict: terminated by other getUpdates request".

Every request/response is appended to the log file as one TSV line so the
number of *concurrent* pollers can be counted afterwards: each poller has
its own reqwest connection pool, hence its own TCP source port.

Usage:  fake_telegram_api.py [port] [logfile]
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
LOGFILE = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fake_tg.log"
CONFLICT_DELAY = 1.0  # seconds a displaced long-poll waits before its 409

T0 = time.time()
_log_lock = threading.Lock()


def log(*fields):
    line = "\t".join([f"{time.time() - T0:9.3f}"] + [str(f) for f in fields])
    with _log_lock:
        with open(LOGFILE, "a") as fh:
            fh.write(line + "\n")
            fh.flush()


class Queue:
    """Single-reader update queue: tracks who currently holds getUpdates."""

    def __init__(self):
        self.lock = threading.Lock()
        self.holder = None  # dict(id=, evicted=threading.Event())
        self.seq = 0

    def acquire(self):
        with self.lock:
            self.seq += 1
            me = {"id": self.seq, "evicted": threading.Event()}
            old = self.holder
            self.holder = me
            if old is not None:
                old["evicted"].set()  # newcomer wins, incumbent is terminated
            return me, (old["id"] if old else None)

    def release(self, me):
        with self.lock:
            if self.holder is me:
                self.holder = None


QUEUE = Queue()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence stderr access log
        pass

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if not n:
            return {}
        raw = self.rfile.read(n)
        try:
            return json.loads(raw)
        except Exception:
            return {}

    def _reply(self, code, obj):
        payload = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _handle(self):
        params = self._body()
        method = self.path.rsplit("/", 1)[-1].split("?")[0]
        port = self.client_address[1]

        if method == "getMe":
            log("getMe", port)
            return self._reply(200, {"ok": True, "result": {
                "id": 111111, "is_bot": True, "first_name": "stub",
                "username": "stub_probe_bot"}})
        if method in ("deleteWebhook", "setMyCommands", "getWebhookInfo"):
            log(method, port)
            return self._reply(200, {"ok": True, "result": True})
        if method != "getUpdates":
            log("other:" + method, port)
            return self._reply(200, {"ok": True, "result": []})

        timeout = float(params.get("timeout", 0) or 0)
        me, displaced = QUEUE.acquire()
        log("getUpdates.start", port, f"req={me['id']}", f"timeout={timeout}",
            f"displaced={displaced}")
        evicted = me["evicted"].wait(timeout=timeout if timeout else 0.001)
        if evicted:
            time.sleep(CONFLICT_DELAY)
            log("getUpdates.409", port, f"req={me['id']}")
            return self._reply(409, {
                "ok": False, "error_code": 409,
                "description": "Conflict: terminated by other getUpdates "
                               "request; make sure that only one bot instance "
                               "is running"})
        QUEUE.release(me)
        log("getUpdates.200", port, f"req={me['id']}")
        return self._reply(200, {"ok": True, "result": []})

    do_GET = _handle
    do_POST = _handle


if __name__ == "__main__":
    if os.path.exists(LOGFILE):
        os.remove(LOGFILE)
    log("stub.start", f"port={PORT}")
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
