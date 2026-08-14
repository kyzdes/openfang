#!/usr/bin/env python3
"""FANG-52 harness: fake Telegram Bot API.

Superset of two prior probes, kept nowhere else but here:
  * tests/fang/fake_telegram_api.py  — the single-reader getUpdates queue
    semantics (409 "Conflict: terminated by other getUpdates request" when a
    second poller shows up), which caught FANG-31.
  * tests/fang/FANG-43.sh's inline stub — getFile + the /file/bot<token>/...
    raw download route, which caught FANG-43 (bot token leaking into an LLM
    prompt via a file URL).
Neither original file is modified; see tests/fang/harness/README.md for the
"what's from where" mapping.

Usage:  tg_stub.py <scenario.json> <rig-dir>

The scenario's top-level "telegram" object drives this (see README for the
schema). Update queue can be extended at runtime by POSTing a JSON body to
this process's own /__push endpoint (used by `fangrig tg-send`).

Never makes outgoing network calls of its own — selftest greps this file
for that.
"""
import hashlib
import json
import os
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BIND_HOST = os.environ.get("FANGRIG_BIND_HOST", "127.0.0.1")


def die(code, msg):
    sys.stderr.write("tg_stub: %s\n" % msg)
    sys.stderr.flush()
    sys.exit(code)


if len(sys.argv) < 3:
    die(4, "usage: tg_stub.py <scenario.json> <rig-dir>")

SCEN_PATH, RIG_DIR = sys.argv[1], sys.argv[2]
with open(SCEN_PATH) as fh:
    SCEN = json.load(fh)

TG = SCEN.get("telegram") or {}
RUN_ID = SCEN.get("run_id", "unknown")
PORT = TG.get("port", 8981)
TOKEN = TG.get("token", "424242:FANGRIG-fake-token-do-not-reuse")
QUEUE_POLICY = TG.get("queue_policy", "permissive")  # "permissive" | "single_reader"
CONFLICT_DELAY = float(TG.get("conflict_delay", 1.0))
TOKEN_SHA = hashlib.sha256(TOKEN.encode()).hexdigest()[:8]

JOURNAL_PATH = os.path.join(RIG_DIR, "tg-journal.jsonl")
T0 = time.time()

_jlock = threading.Lock()


def jwrite(rec):
    line = json.dumps(rec, sort_keys=True)
    with _jlock:
        with open(JOURNAL_PATH, "a") as fh:
            fh.write(line + "\n")
            fh.flush()


_update_id = [1000000]
_update_lock = threading.Lock()


def next_update_id():
    with _update_lock:
        _update_id[0] += 1
        return _update_id[0]


# A fixed chat_id (555555, matching FANG-31/FANG-43's stubs) collides with
# whatever agent-binding those earlier probes left behind in the staging
# volume's persistent chat->agent binding table — a *later* run's messages
# then silently route to that stale binding (a real agent, a real LLM,
# real cost) instead of to the run's own fangrig-* agent, no matter what
# default_agent says. Derive a chat_id from run_id instead so every run
# gets its own, never-seen-before chat identity. Scenarios/`tg-send` can
# still override it explicitly via "chat_id" if a fixed value is wanted.
DEFAULT_CHAT_ID = 700_000_000 + (int(hashlib.sha256(RUN_ID.encode()).hexdigest(), 16) % 90_000_000)


def build_update(spec):
    """Turn a scenario/`tg-send` update spec into a Telegram Update object."""
    kind = spec.get("kind", "text")
    chat_id = spec.get("chat_id", DEFAULT_CHAT_ID)
    base_msg = {
        "message_id": spec.get("message_id", next_update_id() % 100000),
        "date": int(time.time()),
        "chat": {"id": chat_id, "type": "private"},
        "from": {"id": chat_id, "is_bot": False, "first_name": "Probe"},
    }
    if kind == "text":
        base_msg["text"] = spec.get("text", "probe")
    elif kind == "document":
        base_msg["document"] = {
            "file_id": spec.get("file_id", "FAKE_FILE_ID_DOC"),
            "file_name": spec.get("file_name", "probe-secret-exfil.pdf"),
            "mime_type": spec.get("mime_type", "application/pdf"),
            "file_size": len(spec.get("bytes", "probe")),
        }
    elif kind == "photo":
        base_msg["photo"] = [{
            "file_id": spec.get("file_id", "FAKE_FILE_ID_PHOTO"),
            "file_size": len(spec.get("bytes", "probe")),
            "width": 100, "height": 100,
        }]
    elif kind == "voice":
        base_msg["voice"] = {
            "file_id": spec.get("file_id", "FAKE_FILE_ID_VOICE"),
            "duration": spec.get("duration", 3),
            "mime_type": "audio/ogg",
        }
    else:
        base_msg["text"] = spec.get("text", "probe")
    return {"update_id": next_update_id(), "message": base_msg}


UPDATE_QUEUE = [build_update(u) for u in TG.get("updates", [])]
QUEUE_LOCK = threading.Lock()

# file_id -> (file_path, bytes)
FILES = {}
for u in TG.get("updates", []):
    kind = u.get("kind")
    if kind == "document":
        fid = u.get("file_id", "FAKE_FILE_ID_DOC")
        FILES[fid] = ("documents/" + u.get("file_name", "probe-secret-exfil.pdf"),
                       u.get("bytes", "not a real pdf, just probe bytes").encode())
    elif kind == "photo":
        fid = u.get("file_id", "FAKE_FILE_ID_PHOTO")
        FILES[fid] = ("photos/" + u.get("file_name", "probe.jpg"),
                       u.get("bytes", "not a real jpg, just probe bytes").encode())
    elif kind == "voice":
        fid = u.get("file_id", "FAKE_FILE_ID_VOICE")
        FILES[fid] = ("voice/" + u.get("file_name", "probe.ogg"),
                       u.get("bytes", "not a real ogg, just probe bytes").encode())

# ---- single-reader queue-holder semantics (from fake_telegram_api.py) ------
class Holder:
    def __init__(self):
        self.lock = threading.Lock()
        self.current = None
        self.seq = 0

    def acquire(self):
        with self.lock:
            self.seq += 1
            me = {"id": self.seq, "evicted": threading.Event()}
            old = self.current
            self.current = me
            if old is not None:
                old["evicted"].set()
            return me, (old["id"] if old else None)

    def release(self, me):
        with self.lock:
            if self.current is me:
                self.current = None


HOLDER = Holder()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            return json.loads(raw) if raw else {}
        except Exception:
            return {}

    def _reply(self, code, obj):
        payload = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _reply_bytes(self, code, data, content_type="application/octet-stream"):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def _route(self, method):
        path = self.path.split("?")[0]

        if path == "/__push" and method == "POST":
            spec = self._body()
            with QUEUE_LOCK:
                UPDATE_QUEUE.append(build_update(spec))
            return self._reply(200, {"ok": True})
        if path == "/__health":
            return self._reply(200, {"ok": True, "run_id": RUN_ID, "port": PORT})

        # /file/bot<token>/<path> — raw file download. Matched BEFORE the
        # /bot<token>/<method> route (FANG-43's grabs: a loose method regexp
        # here would swallow the file path as a bogus "method").
        m_file = re.match(r"^/file/bot([^/]+)/(.+)$", path)
        if m_file:
            token_in_url = m_file.group(1)
            file_path = m_file.group(2)
            body = self._body()
            jwrite({
                "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "t_rel": round(time.time() - T0, 3),
                "method": "file_download",
                "token_in_url_sha256_8": hashlib.sha256(token_in_url.encode()).hexdigest()[:8],
                "full_token_seen": token_in_url == TOKEN,
                "chat_id": None, "update_id": None,
                "file_path": file_path,
            })
            for _fid, (fpath, data) in FILES.items():
                if fpath == file_path:
                    return self._reply_bytes(200, data)
            return self._reply_bytes(404, b"not found")

        m = re.match(r"^/bot([^/]+)/(\w+)$", path)
        if not m:
            self._body()
            return self._reply(404, {"ok": False, "description": "fangrig: no such route"})
        token_in_url, meth = m.group(1), m.group(2)
        body = self._body()

        def log(extra=None):
            rec = {
                "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "t_rel": round(time.time() - T0, 3),
                "method": meth,
                "token_in_url_sha256_8": hashlib.sha256(token_in_url.encode()).hexdigest()[:8],
                "full_token_seen": token_in_url == TOKEN,
                "chat_id": body.get("chat_id"),
                "update_id": None,
            }
            if extra:
                rec.update(extra)
            jwrite(rec)

        if meth == "getMe":
            log()
            return self._reply(200, {"ok": True, "result": {
                "id": 1, "is_bot": True, "first_name": "fangrig", "username": "fangrig_probe_bot"}})
        if meth in ("deleteWebhook", "setWebhook", "setMyCommands", "sendChatAction", "setMessageReaction"):
            log()
            return self._reply(200, {"ok": True, "result": True})
        if meth == "getFile":
            fid = body.get("file_id")
            entry = FILES.get(fid)
            log()
            if entry:
                fpath, _data = entry
                return self._reply(200, {"ok": True, "result": {"file_id": fid, "file_path": fpath}})
            return self._reply(200, {"ok": True, "result": {"file_id": fid, "file_path": "unknown/" + str(fid)}})
        if meth in ("sendMessage", "sendDocument", "sendPhoto"):
            log({"body": body})
            return self._reply(200, {"ok": True, "result": {
                "message_id": next_update_id() % 100000,
                "date": int(time.time()),
                "chat": {"id": body.get("chat_id", 0), "type": "private"},
                "text": body.get("text", ""),
            }})
        if meth == "getUpdates":
            return self._get_updates(body, log)

        log()
        return self._reply(200, {"ok": True, "result": []})

    def _get_updates(self, body, log):
        timeout = float(body.get("timeout", 0) or 0)
        if QUEUE_POLICY == "single_reader":
            me, displaced = HOLDER.acquire()
            log({"policy": "single_reader", "req": me["id"], "displaced": displaced})
            evicted = me["evicted"].wait(timeout=timeout if timeout else 0.05)
            if evicted:
                time.sleep(CONFLICT_DELAY)
                return self._reply(409, {
                    "ok": False, "error_code": 409,
                    "description": "Conflict: terminated by other getUpdates request; "
                                    "make sure that only one bot instance is running"})
            HOLDER.release(me)

        with QUEUE_LOCK:
            if UPDATE_QUEUE:
                upd = UPDATE_QUEUE.pop(0)
                log({"delivered_update_id": upd["update_id"]})
                return self._reply(200, {"ok": True, "result": [upd]})

        # empty long-poll: sleep up to min(timeout, 5) like a real long-poll,
        # then answer with no updates.
        time.sleep(min(timeout, 5.0) if timeout else 0.05)
        return self._reply(200, {"ok": True, "result": []})


def main():
    srv = ThreadingHTTPServer((BIND_HOST, PORT), Handler)
    srv.daemon_threads = True
    sys.stderr.write("tg_stub: listening %s:%d run_id=%s policy=%s queued=%d\n"
                      % (BIND_HOST, PORT, RUN_ID, QUEUE_POLICY, len(UPDATE_QUEUE)))
    sys.stderr.flush()
    srv.serve_forever()


if __name__ == "__main__":
    main()
