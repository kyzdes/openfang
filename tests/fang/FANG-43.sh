#!/usr/bin/env bash
# FANG-43 — the Telegram bot token leaks into the LLM prompt (and thence into
# the on-disk session history) whenever a user sends a file/photo/voice message.
#
# Root cause: crates/openfang-channels/src/telegram.rs:910 builds the download
# URL as `{api_base_url}/file/bot{token}/{file_path}` — i.e. WITH the live bot
# token baked into the URL string, because that's how Telegram's Bot API
# authenticates file downloads. That URL is stored verbatim in
# ChannelContent::File/Voice{ url, .. } (types.rs), and bridge.rs formats it
# straight into the prompt text it sends to the LLM:
#   bridge.rs:1041  format!("[User sent a file ({filename}): {url}]")
#   bridge.rs:1047  format!("[User sent a voice message ({duration_seconds}s): {url}]")
# (plus the Multipart-path duplicates at bridge.rs:922/931/1064/1069, and the
# Image path at bridge.rs:1032/1033 which is ONLY reached when the image byte
# download for vision blocks fails — File/Voice have no such byte-download
# fallback, so they ALWAYS go out as a bare URL with the token in it).
#
# That prompt text becomes a "role":"user" turn in the agent's canonical
# session, which openfang-kernel persists to disk after the turn completes
# (kernel.rs: memory.append_canonical + write_jsonl_mirror), i.e. the full
# token sits in cleartext in:
#   /data/workspaces/<agent>/sessions/<uuid>.jsonl
# AND was sent in cleartext inside the chat-completion request body to
# whatever LLM provider is configured — so a compromised or just plain
# untrusted LLM provider learns a token that lets it read (and, depending on
# Telegram file-type, exfiltrate) any file the bot can access.
#
# This script proves it live: it stands up a fake Telegram Bot API stub
# INSIDE the staging container's network namespace (nsenter, same pattern as
# FANG-31.sh — the real Telegram channel stays disabled and no traffic to
# api.telegram.org is involved), points `[channels.telegram]` at it with a
# throwaway fake token, sends one `document` update through it, and then
# reads the resulting session file straight off the staging data volume to
# confirm the fake token landed there whole.
#
# Usage:  ./FANG-43.sh [base_url]      default: $OPENFANG_URL or staging
# Idempotent: restores config.toml and kills the stub on every exit path.

set -uo pipefail

BASE_URL="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
CONTAINER="${OF_CONTAINER:-openfang-staging}"
CONFIG="${OF_CONFIG:-/var/lib/docker/volumes/openfang-staging-data/_data/config.toml}"
STUB_PORT="${OF_STUB_PORT:-8097}"
FAKE_TOKEN="424242:FANG43-fake-token-do-not-reuse-ABCDEF"
AGENT_NAME="assistant"
WORK="$(mktemp -d /tmp/fang43.XXXXXX)"

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production." >&2; exit 2;;
esac
if [ "$CONTAINER" = "openfang-openfang-1" ]; then
  echo "REFUSING: $CONTAINER is production." >&2; exit 2
fi
for bin in docker nsenter python3 curl; do
  command -v "$bin" >/dev/null || { echo "missing dependency: $bin" >&2; exit 3; }
done
[ -f "$CONFIG" ] || { echo "no config at $CONFIG" >&2; exit 3; }

API_KEY="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" | head -1)"
NETPID="$(docker inspect -f '{{.State.Pid}}' "$CONTAINER")" || exit 3
SECRETS="$(dirname "$CONFIG")/secrets.env"
STATE_DIR="$(dirname "$CONFIG")/workspaces/$AGENT_NAME/sessions"

api() {
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -sS -m 120 -X "$m" -H 'Content-Type: application/json' \
         ${API_KEY:+-H "Authorization: Bearer $API_KEY"} -d "$b" "$BASE_URL$p"
  else
    curl -sS -m 120 -X "$m" ${API_KEY:+-H "Authorization: Bearer $API_KEY"} "$BASE_URL$p"
  fi
}

cleanup() {
  [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null
  if [ -f "$WORK/config.toml.orig" ]; then
    cp "$WORK/config.toml.orig" "$CONFIG"
    api POST /api/channels/reload '{}' >/dev/null 2>&1
    echo "restored $CONFIG"
  fi
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- the stub --
# Implements getMe/deleteWebhook/setMyCommands (adapter startup), one
# getUpdates delivering a `document` message, getFile resolving it, and the
# /file/bot<token>/<path> download endpoint — enough for
# telegram_get_file_url() to succeed and embed the token in the URL exactly
# as the real Bot API would.
cat > "$WORK/stub.py" <<PY
import json, re, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
SENT = threading.Event()
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def _json(self, o, code=200):
        b = json.dumps(o).encode(); self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers()
        self.wfile.write(b)
    def handle_one(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        sys.stderr.write("REQ %s\n" % self.path); sys.stderr.flush()
        # /file/bot<token>/<path> — raw file download, checked first (no /bot<...>/<method> match)
        mf = re.match(r"/file/bot([^/]+)/(.+)", self.path)
        if mf:
            sys.stderr.write("  -> file download, token-in-url=%s path=%s\n" % (mf.group(1), mf.group(2)))
            sys.stderr.flush()
            data = b"not a real pdf, just probe bytes"
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        m = re.match(r"/bot([^/]+)/(\w+)", self.path)
        meth = m.group(2) if m else "?"
        sys.stderr.write("  -> method=%s\n" % meth); sys.stderr.flush()
        if meth == "getMe":
            return self._json({"ok": True, "result": {"id": 1, "is_bot": True,
                    "username": "fang43_probe_bot", "first_name": "Fang43"}})
        if meth in ("deleteWebhook", "setMyCommands", "setWebhook"):
            return self._json({"ok": True, "result": True})
        if meth == "getFile":
            return self._json({"ok": True, "result": {
                "file_id": "FAKE_FILE_ID_FANG43", "file_size": 33,
                "file_path": "documents/probe-secret-exfil.pdf"}})
        if meth == "getUpdates":
            if not SENT.is_set():
                SENT.set()
                sys.stderr.write("  -> delivering probe document update\n"); sys.stderr.flush()
                return self._json({"ok": True, "result": [{
                    "update_id": 900001,
                    "message": {
                        "message_id": 1,
                        "date": int(time.time()),
                        "chat": {"id": 555555, "type": "private"},
                        "from": {"id": 555555, "is_bot": False, "first_name": "Probe"},
                        "document": {
                            "file_id": "FAKE_FILE_ID_FANG43",
                            "file_name": "probe-secret-exfil.pdf",
                            "mime_type": "application/pdf",
                            "file_size": 33
                        }
                    }
                }]})
            # subsequent long-polls: emulate real long-poll latency then empty
            try:
                p = json.loads(body) if body else {}
                t = min(float(p.get("timeout", 0) or 0), 5.0)
            except Exception:
                t = 0.0
            time.sleep(t)
            return self._json({"ok": True, "result": []})
        return self._json({"ok": True, "result": []})
    do_GET = do_POST = handle_one
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

echo "=== FANG-43 · Telegram bot token leaks into the LLM prompt via file URLs ==="
echo "date              : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "target            : $BASE_URL  (container $CONTAINER)"
echo "daemon version    : $(api GET /api/health | python3 -c 'import sys,json;print(json.load(sys.stdin).get("version","?"))' 2>/dev/null)"
echo "image             : $(docker inspect -f '{{.Image}}' "$CONTAINER" | cut -c1-19)"
echo "fake token        : $FAKE_TOKEN"
echo "target agent      : $AGENT_NAME  (session dir: $STATE_DIR)"
echo

cp "$CONFIG" "$WORK/config.toml.orig"
grep -q '^FANG43_FAKE_TG_TOKEN=' "$SECRETS" 2>/dev/null || \
  echo "FANG43_FAKE_TG_TOKEN=$FAKE_TOKEN" >> "$SECRETS"

python3 - "$CONFIG" "$STUB_PORT" "$AGENT_NAME" <<'PY'
import re, sys
path, port, agent = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(path).read()
s = re.sub(r"(?ms)^\[channels\.telegram\].*?(?=^\[|\Z)", "", s)
s += ('\n[channels.telegram]\nbot_token_env = "FANG43_FAKE_TG_TOKEN"\n'
      'poll_interval_secs = 1\napi_url = "http://127.0.0.1:%s"\n'
      'default_agent = "%s"\n' % (port, agent))
open(path, "w").write(s)
PY

# Snapshot which session files this agent already has, so we can spot the new one.
BEFORE_SESSIONS="$(ls "$STATE_DIR" 2>/dev/null | sort)"

setsid nsenter -t "$NETPID" -n python3 "$WORK/stub.py" "$STUB_PORT" \
  >"$WORK/stub.err" 2>&1 < /dev/null &
STUB_PID=$!
sleep 1

echo "--- POST /api/channels/reload (attach poller to the stub) ---"
api POST /api/channels/reload '{}'; echo
echo

echo "--- waiting for the agent turn to complete (LLM call + session write) ---"
NEW_FILE=""
for _ in $(seq 60); do
    AFTER_SESSIONS="$(ls "$STATE_DIR" 2>/dev/null | sort)"
    NEW_FILE="$(comm -13 <(echo "$BEFORE_SESSIONS") <(echo "$AFTER_SESSIONS") | head -1)"
    if [ -n "$NEW_FILE" ] && grep -q 'probe-secret-exfil.pdf' "$STATE_DIR/$NEW_FILE" 2>/dev/null; then
        break
    fi
    # also allow the case where the document landed in an EXISTING session file
    for f in $AFTER_SESSIONS; do
        if grep -q 'probe-secret-exfil.pdf' "$STATE_DIR/$f" 2>/dev/null; then
            NEW_FILE="$f"; break 2
        fi
    done
    sleep 2
done

echo "stub log (stderr, should show getMe/getUpdates/getFile/file download):"
cat "$WORK/stub.err"
echo

if [ -z "$NEW_FILE" ] || [ ! -f "$STATE_DIR/$NEW_FILE" ]; then
    echo "RESULT: INCONCLUSIVE — no session file received the probe document within timeout."
    echo "(daemon log tail follows for diagnosis)"
    docker logs --timestamps --since 3m "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | tail -60
    exit 1
fi

echo "--- session file: $STATE_DIR/$NEW_FILE ---"
LEAK_LINE="$(grep -n 'probe-secret-exfil.pdf' "$STATE_DIR/$NEW_FILE" | head -1)"
echo "$LEAK_LINE"
echo

FULL_TOKEN_HITS="$(grep -c "$FAKE_TOKEN" "$STATE_DIR/$NEW_FILE" 2>/dev/null || echo 0)"
echo "FANG43_FULL_TOKEN_OCCURRENCES_IN_SESSION=$FULL_TOKEN_HITS   # want 0"

if [ "$FULL_TOKEN_HITS" -gt 0 ]; then
    echo "RESULT: RED — the full bot token is present verbatim in the on-disk session"
    echo "        (and therefore was sent verbatim inside the LLM chat-completion request)."
else
    echo "RESULT: GREEN — token not found verbatim in the session file."
fi
