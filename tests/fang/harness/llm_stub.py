#!/usr/bin/env python3
"""FANG-52 harness: fake OpenAI-compatible LLM provider.

Multi-endpoint (multiple "roles", each its own TCP port and OpenAI-style
prefix), scripted by a scenario JSON file, journalling every request to a
JSONL file on the host so a run's evidence survives even if the stub
container is killed. See tests/fang/harness/README.md for the scenario
schema and tests/fang/harness/lib.sh for how this process gets launched.

Usage:  llm_stub.py <scenario.json> <rig-dir>

Never makes outgoing network calls of its own — selftest greps this file
for that (tests/fang/harness/README.md, "contrôle d'isolation").
"""
import hashlib
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT_MIN, PORT_MAX = 8971, 8979
BIND_HOST = os.environ.get("FANGRIG_BIND_HOST", "127.0.0.1")


def die(code, msg):
    sys.stderr.write("llm_stub: %s\n" % msg)
    sys.stderr.flush()
    sys.exit(code)


if len(sys.argv) < 3:
    die(4, "usage: llm_stub.py <scenario.json> <rig-dir>")

SCEN_PATH, RIG_DIR = sys.argv[1], sys.argv[2]
try:
    with open(SCEN_PATH) as fh:
        SCEN = json.load(fh)
except Exception as e:  # noqa: BLE001 - report and die, this is a stub
    die(4, "cannot read scenario %s: %s" % (SCEN_PATH, e))

RUN_ID = SCEN.get("run_id", "unknown")
CANARY = "of-fangrig-canary-%s" % RUN_ID
JOURNAL_PATH = os.path.join(RIG_DIR, "journal.jsonl")
T0 = time.time()

# ---------------------------------------------------------------- journal --
_jlock = threading.Lock()


def jwrite(rec):
    line = json.dumps(rec, sort_keys=True)
    with _jlock:
        with open(JOURNAL_PATH, "a") as fh:
            fh.write(line + "\n")
            fh.flush()


_seqlock = threading.Lock()
_seq = [0]


def next_seq():
    with _seqlock:
        _seq[0] += 1
        return _seq[0]


_rollup_lock = threading.Lock()
ROLLUP = {"counts": {}, "models": {}, "streams": {}, "statuses": {}, "log": []}


def rollup_note(role, model, stream, status, step_type, note=None):
    with _rollup_lock:
        ROLLUP["counts"][role] = ROLLUP["counts"].get(role, 0) + 1
        if model:
            ROLLUP["models"][model] = ROLLUP["models"].get(model, 0) + 1
        if stream:
            ROLLUP["streams"][role] = ROLLUP["streams"].get(role, 0) + 1
        skey = str(status)
        ROLLUP["statuses"][skey] = ROLLUP["statuses"].get(skey, 0) + 1
        ROLLUP["log"].append(
            note or ("%s call#%d model=%s step=%s"
                     % (role, ROLLUP["counts"][role], model, step_type))
        )


def rollup_note_raw(line):
    with _rollup_lock:
        ROLLUP["log"].append(line)


def rollup_reset():
    with _rollup_lock:
        ROLLUP["counts"] = {}
        ROLLUP["models"] = {}
        ROLLUP["streams"] = {}
        ROLLUP["statuses"] = {}
        ROLLUP["log"] = []


# ------------------------------------------------------------ role state --
ROLES = {}        # role -> endpoint config dict (with cached "_flat" steps)
PORT_ROLES = {}    # port -> [role, ...]
ROLE_OPEN = {}     # role -> bool
ROLE_CALLS = {}    # role -> int, calls served so far (for step selection)
PORT_SERVERS = {}  # port -> (HTTPServer, thread)
STATE_LOCK = threading.RLock()


def flatten_steps(steps):
    flat = []
    for orig_idx, step in enumerate(steps):
        repeat = int(step.get("repeat", 1))
        for r in range(1, max(repeat, 1) + 1):
            flat.append((orig_idx, step, r))
    return flat


for ep in SCEN.get("endpoints", []):
    role = ep.get("role")
    if not role:
        die(4, "endpoint missing 'role'")
    if role in ROLES:
        die(4, "duplicate role in scenario: %s" % role)
    port = ep.get("port")
    if port is None or not (PORT_MIN <= port <= PORT_MAX):
        die(4, "role %s: port %r outside %d-%d" % (role, port, PORT_MIN, PORT_MAX))
    ep.setdefault("prefix", "")
    ep["_flat"] = flatten_steps(ep.get("steps", []))
    ROLES[role] = ep
    PORT_ROLES.setdefault(port, []).append(role)
    ROLE_OPEN[role] = bool(ep.get("open", True))
    ROLE_CALLS[role] = 0

if not ROLES:
    die(4, "scenario has no endpoints")


def substitute(s, extra=None):
    if not isinstance(s, str):
        return s
    vals = {"run_id": RUN_ID, "canary": CANARY}
    if extra:
        vals.update(extra)
    out = s
    for k, v in vals.items():
        out = out.replace("{{%s}}" % k, str(v))
    return out


def substitute_deep(obj, extra=None):
    if isinstance(obj, str):
        return substitute(obj, extra)
    if isinstance(obj, dict):
        return {k: substitute_deep(v, extra) for k, v in obj.items()}
    if isinstance(obj, list):
        return [substitute_deep(v, extra) for v in obj]
    return obj


def usage_obj(u):
    i, o = (u or [0, 0])
    return {"prompt_tokens": i, "completion_tokens": o, "total_tokens": i + o}


def key_echo_value(received_token):
    # SECURITY (spec 4.4.2): only ever echoes the *harness canary* back.
    # Any other token — including a real provider key that leaked in by
    # mistake — is replaced before it can appear in a response body.
    if received_token == CANARY:
        return CANARY
    return "sk-...REDACTED-BY-FANGRIG"


# --------------------------------------------------------------- serving --
class RigHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence default stderr access log
        pass

    # -- helpers --------------------------------------------------------
    def _read_body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def _send_json(self, code, obj):
        payload = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _send_raw(self, code, raw_text, content_type="application/json"):
        payload = raw_text.encode()
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _write_chunk(self, data: bytes):
        self.wfile.write(("%x\r\n" % len(data)).encode() + data + b"\r\n")
        self.wfile.flush()

    def _write_chunk_end(self):
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _match_role(self, path):
        roles = self.server.roles
        best, best_len = None, -1
        for r in roles:
            prefix = ROLES[r].get("prefix", "")
            if path.startswith(prefix) and len(prefix) > best_len:
                best, best_len = r, len(prefix)
        return best

    # -- routing ----------------------------------------------------------
    def do_GET(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def _route(self, method):
        path = self.path.split("?")[0]
        if path == "/__health":
            return self._health()
        if path == "/__journal" and method == "GET":
            return self._journal_get()
        if path == "/__journal/reset" and method == "POST":
            self._read_body()
            rollup_reset()
            return self._send_json(200, {"ok": True})

        role = self._match_role(path)
        if role is None:
            self._read_body()
            return self._send_json(404, {"error": {"message": "fangrig: no endpoint for " + path}})
        ep = ROLES[role]

        if path.endswith("/models"):
            self._read_body()
            return self._send_json(200, {
                "object": "list",
                "data": [{"id": "fangrig-%s" % role, "object": "model", "owned_by": "fangrig"}],
            })
        if path.endswith("/chat/completions") and method == "POST":
            return self._chat(role, ep)

        self._read_body()
        return self._send_json(404, {"error": {"message": "fangrig: unknown path " + path}})

    def _health(self):
        self._read_body()
        obj = {
            "ok": True,
            "run_id": RUN_ID,
            "scenario": SCEN.get("name"),
            "endpoints": [
                {"role": r, "port": ROLES[r]["port"], "open": ROLE_OPEN[r]}
                for r in ROLES
            ],
        }
        self._send_json(200, obj)

    def _journal_get(self):
        self._read_body()
        with _rollup_lock:
            obj = {
                "run_id": RUN_ID,
                "scenario": SCEN.get("name"),
                "counts": dict(ROLLUP["counts"]),
                "models": dict(ROLLUP["models"]),
                "streams": dict(ROLLUP["streams"]),
                "statuses": dict(ROLLUP["statuses"]),
                "log": list(ROLLUP["log"]),
            }
        self._send_json(200, obj)

    # -- the actual chat/completions endpoint ----------------------------
    def _chat(self, role, ep):
        raw = self._read_body()
        try:
            body = json.loads(raw) if raw else {}
        except Exception:
            body = {}

        auth_header = self.headers.get("Authorization")
        present = bool(auth_header)
        scheme, token = None, None
        if present:
            parts = auth_header.split(" ", 1)
            scheme = parts[0]
            token = parts[1] if len(parts) > 1 else ""
        token_sha = hashlib.sha256(token.encode()).hexdigest()[:8] if token else None
        is_harness_key = token == CANARY
        auth_meta = {
            "present": present,
            "scheme": scheme,
            "len": len(token) if token else 0,
            "sha256_8": token_sha,
            "is_harness_key": is_harness_key,
        }

        messages = body.get("messages") or []
        last_role = messages[-1].get("role") if messages else None
        first_user_text = ""
        for m in messages:
            if m.get("role") == "user":
                c = m.get("content")
                if isinstance(c, str):
                    first_user_text = c[:200]
                elif isinstance(c, list):
                    first_user_text = json.dumps(c)[:200]
                break
        tools = [t.get("function", {}).get("name") for t in (body.get("tools") or [])]
        stream_req = bool(body.get("stream"))
        has_stream_options = "stream_options" in body
        model = body.get("model", "")
        req_sha = hashlib.sha256(raw).hexdigest()[:8]

        seq = next_seq()
        t_rel = round(time.time() - T0, 3)
        base_rec = {
            "seq": seq,
            "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "t_rel": t_rel,
            "endpoint": role,
            "port": ep["port"],
            "path": self.path.split("?")[0],
            "method": "POST",
            "stream": stream_req,
            "model": model,
            "n_messages": len(messages),
            "last_role": last_role,
            "tools": tools,
            "tool_choice": body.get("tool_choice"),
            "max_tokens": body.get("max_tokens"),
            "temperature": body.get("temperature"),
            "has_stream_options": has_stream_options,
            "auth": auth_meta,
            "first_user_text": first_user_text,
            "request_sha256_8": req_sha,
        }
        jwrite(dict(base_rec, phase="recv"))
        t_start = time.time()

        require_auth = bool(ep.get("require_auth"))
        if require_auth and not present:
            self._send_json(401, {"error": {"message": "fangrig: missing api key", "type": "invalid_request_error"}})
            jwrite(dict(base_rec, phase="done", step_index=-1, step_type="auth_reject",
                        repeat_hit=0, status=401, duration_ms=int((time.time() - t_start) * 1000)))
            rollup_note(role, model, stream_req, 401, "auth_reject")
            return

        quirks = ep.get("stream_quirks", [])
        if "reject_stream_options" in quirks and has_stream_options:
            self._send_json(400, {"error": {"message": "fangrig: stream_options not supported", "type": "invalid_request_error"}})
            jwrite(dict(base_rec, phase="done", step_index=-1, step_type="quirk_reject_stream_options",
                        repeat_hit=0, status=400, duration_ms=int((time.time() - t_start) * 1000)))
            rollup_note(role, model, stream_req, 400, "quirk_reject_stream_options")
            return

        # `fangrig selftest` fires diagnostic probes (model names prefixed
        # "selftest-", case-insensitive) at whatever role/port it's given.
        # Those must never advance the scenario's own step cursor, or a
        # `selftest` run (which `up` runs automatically) silently eats one
        # or more of the scenario's real steps before the actual agent turn
        # ever happens — exactly the kind of self-inflicted, hard-to-spot
        # bug this harness exists to catch in the *product*, not cause in
        # itself. So: served out of band, off a synthetic step, never
        # touching ROLE_CALLS.
        if model.lower().startswith("selftest-"):
            extra_sub = {"role": role, "seq": str(seq), "key_echo": key_echo_value(token)}
            diag_step = {"type": "text", "content": "fangrig-selftest-ack:%s" % model, "usage": [1, 1]}
            status_code = self._dispatch_step(diag_step, "text", role, ep, model, seq, stream_req,
                                               has_stream_options, body, extra_sub)
            jwrite(dict(base_rec, phase="done", step_index=-2, step_type="selftest_diagnostic",
                        repeat_hit=0, status=status_code, duration_ms=int((time.time() - t_start) * 1000)))
            # Deliberately NOT rollup_note()'d: the rollup's counts/models/
            # streams/statuses are what acceptance checks like "split-turn
            # gives counts:{primary:1,fallback:1}" read, and a selftest
            # probe (which `up` fires automatically) is not scenario
            # traffic. It's still in journal.jsonl above, in full, for
            # anyone auditing what actually hit the wire.
            return

        with STATE_LOCK:
            ROLE_CALLS[role] += 1
            call_index = ROLE_CALLS[role]
        flat = ep["_flat"]
        if call_index <= len(flat):
            orig_idx, step, repeat_hit = flat[call_index - 1]
        else:
            orig_idx, repeat_hit = -1, 0
            step = ep.get("default_step", {"type": "text", "content": "FANGRIG-EXHAUSTED", "usage": [1, 1]})
        step_type = step.get("type", "text")
        extra_sub = {"role": role, "seq": str(seq), "key_echo": key_echo_value(token)}

        status_code = None
        try:
            status_code = self._dispatch_step(step, step_type, role, ep, model, seq, stream_req,
                                               has_stream_options, body, extra_sub)
        except (BrokenPipeError, ConnectionResetError):
            status_code = None  # client hung up (e.g. our own 'close'/'hang' step)

        duration_ms = int((time.time() - t_start) * 1000)
        jwrite(dict(base_rec, phase="done", step_index=orig_idx, step_type=step_type,
                    repeat_hit=repeat_hit, status=status_code, duration_ms=duration_ms))
        rollup_note(role, model, stream_req, status_code, step_type)

        then = step.get("then")
        if then:
            for action, target in then.items():
                if action == "open":
                    set_open(target, True)
                    rollup_note_raw("--- then: opened %s ---" % target)
                elif action == "close":
                    set_open(target, False)
                    rollup_note_raw("--- then: closed %s ---" % target)

    # -- step type dispatch ----------------------------------------------
    def _dispatch_step(self, step, step_type, role, ep, model, seq, stream_req,
                        has_stream_options, body, extra_sub):
        wants_usage = (
            stream_req and has_stream_options
            and bool((body.get("stream_options") or {}).get("include_usage"))
        )

        if step_type == "hang":
            seconds = step.get("seconds", 120)
            time.sleep(seconds)
            try:
                self.connection.shutdown(1)  # SHUT_WR: no response, ever
            except OSError:
                pass
            self.close_connection = True
            return None

        if step_type == "close":
            try:
                self.connection.shutdown(1)
            except OSError:
                pass
            self.close_connection = True
            return None

        if step_type == "status":
            code = step.get("code", 500)
            if "body" in step:
                obj = substitute_deep(step["body"], extra_sub)
                self._send_json(code, obj)
            else:
                text = substitute(step.get("body_template", "{}"), extra_sub)
                self._send_raw(code, text)
            return code

        if step_type == "empty_choices":
            obj = {
                "id": "fangrig-%d" % seq, "object": "chat.completion",
                "created": int(time.time()), "model": model,
                "choices": [], "usage": usage_obj(step.get("usage")),
            }
            if stream_req:
                self._stream_wrap(obj, model, seq, terminal_only=True)
            else:
                self._send_json(200, obj)
            return 200

        if step_type == "malformed":
            raw = substitute(step.get("raw", "{not valid json"), extra_sub)
            self._send_raw(200, raw)
            return 200

        if step_type == "text":
            if stream_req:
                self._stream_text(step, model, seq, ep, wants_usage, extra_sub)
            else:
                content = substitute(step.get("content", ""), extra_sub)
                finish = step.get("finish_reason", step.get("finish", "stop"))
                obj = {
                    "id": "fangrig-%d" % seq, "object": "chat.completion",
                    "created": int(time.time()), "model": model,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                                 "finish_reason": finish}],
                    "usage": usage_obj(step.get("usage")),
                }
                self._send_json(200, obj)
            return 200

        if step_type == "tool_call":
            if stream_req:
                self._stream_toolcall(step, model, seq, ep, wants_usage)
            else:
                call_id = step.get("id", "call_%d" % seq)
                obj = {
                    "id": "fangrig-%d" % seq, "object": "chat.completion",
                    "created": int(time.time()), "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": None, "tool_calls": [{
                            "id": call_id, "type": "function",
                            "function": {"name": step["name"], "arguments": json.dumps(step.get("arguments", {}))},
                        }]},
                        "finish_reason": "tool_calls",
                    }],
                    "usage": usage_obj(step.get("usage")),
                }
                self._send_json(200, obj)
            return 200

        # unknown step type — fail loudly rather than silently succeeding
        self._send_json(500, {"error": {"message": "fangrig: unknown step type " + str(step_type)}})
        return 500

    # -- SSE rendering ------------------------------------------------------
    def _sse_headers(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _sse_event(self, obj):
        self._write_chunk(("data: %s\n\n" % json.dumps(obj)).encode())

    def _sse_done(self):
        self._write_chunk(b"data: [DONE]\n\n")
        self._write_chunk_end()

    def _stream_text(self, step, model, seq, ep, wants_usage, extra_sub):
        content = substitute(step.get("content", ""), extra_sub)
        chunk_chars = max(1, int(ep.get("chunk_chars", 8)))
        delay = ep.get("chunk_delay_ms", 0) / 1000.0
        finish = step.get("finish_reason", step.get("finish", "stop"))
        created = int(time.time())
        self._sse_headers()
        self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                          "model": model, "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})
        for i in range(0, len(content), chunk_chars):
            frag = content[i:i + chunk_chars]
            self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                              "model": model, "choices": [{"index": 0, "delta": {"content": frag}, "finish_reason": None}]})
            if delay:
                time.sleep(delay)
        self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                          "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
        if wants_usage:
            self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                              "model": model, "choices": [], "usage": usage_obj(step.get("usage"))})
        self._sse_done()

    def _stream_toolcall(self, step, model, seq, ep, wants_usage):
        call_id = step.get("id", "call_%d" % seq)
        name = step["name"]
        args_str = json.dumps(step.get("arguments", {}))
        mid = max(1, len(args_str) // 2)
        frags = [args_str[:mid], args_str[mid:]] if len(args_str) > 1 else [args_str, ""]
        created = int(time.time())
        self._sse_headers()
        # F14: first chunk MUST carry both id and function.name.
        self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                          "model": model, "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [
                              {"index": 0, "id": call_id, "type": "function", "function": {"name": name, "arguments": ""}}]},
                              "finish_reason": None}]})
        for frag in frags:
            self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                              "model": model, "choices": [{"index": 0, "delta": {"tool_calls": [
                                  {"index": 0, "function": {"arguments": frag}}]}, "finish_reason": None}]})
        self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                          "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
        if wants_usage:
            self._sse_event({"id": "fangrig-%d" % seq, "object": "chat.completion.chunk", "created": created,
                              "model": model, "choices": [], "usage": usage_obj(step.get("usage"))})
        self._sse_done()

    def _stream_wrap(self, obj, model, seq, terminal_only=False):
        # used for empty_choices under stream:true — a real provider would
        # still speak SSE framing even for a degenerate response.
        self._sse_headers()
        self._sse_event(obj)
        self._sse_done()


# ------------------------------------------------------------- lifecycle --
def start_port_server(port, roles):
    srv = RigHTTPServer((BIND_HOST, port), Handler)
    srv.roles = roles
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    PORT_SERVERS[port] = (srv, t)
    sys.stderr.write("llm_stub: listening %s:%d roles=%s\n" % (BIND_HOST, port, roles))
    sys.stderr.flush()


def stop_port_server(port):
    entry = PORT_SERVERS.pop(port, None)
    if entry:
        srv, _t = entry
        srv.shutdown()
        srv.server_close()
        sys.stderr.write("llm_stub: closed port %d\n" % port)
        sys.stderr.flush()


def set_open(role, want):
    with STATE_LOCK:
        if role not in ROLES:
            return
        ROLE_OPEN[role] = want
        port = ROLES[role]["port"]
        roles_on_port = PORT_ROLES[port]
        any_open = any(ROLE_OPEN[r] for r in roles_on_port)
        if any_open and port not in PORT_SERVERS:
            start_port_server(port, roles_on_port)
        elif not any_open and port in PORT_SERVERS:
            stop_port_server(port)


def main():
    for port, roles in PORT_ROLES.items():
        if any(ROLE_OPEN[r] for r in roles):
            try:
                start_port_server(port, roles)
            except OSError as e:
                die(4, "cannot bind %s:%d: %s" % (BIND_HOST, port, e))
    sys.stderr.write("llm_stub: run_id=%s scenario=%s ready\n" % (RUN_ID, SCEN.get("name")))
    sys.stderr.flush()
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    main()
