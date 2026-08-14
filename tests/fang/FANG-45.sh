#!/usr/bin/env bash
# FANG-45 — file_read silently truncates large files to ~30% of the model's
# context window, and gives the caller no way to fetch the rest.
#
# Root cause (recon only — this script measures, it does not fix anything):
#
#   1. tool_file_read() reads the WHOLE file, no size limit of its own:
#        crates/openfang-runtime/src/tool_runner.rs:1379-1388
#          tokio::fs::read_to_string(&resolved)
#      Its input_schema takes only "path" — no offset/limit/range:
#        crates/openfang-runtime/src/tool_runner.rs:567-580
#      So there is NO way to ask for "the rest of the file". Confirmed live
#      below: the model itself reasons "maybe file_read supports offset?
#      Not indicated" and gives up.
#
#   2. The truncation happens one layer up, generically, for every tool
#      result (not file_read-specific):
#        crates/openfang-runtime/src/context_budget.rs
#          per_result_cap()  = 30% of context_window_tokens, at
#                               2.0 chars/token  ==>  cap ≈ 0.6 * ctx_tokens
#          truncate_tool_result_dynamic()  cuts at the last '\n' before cap
#            and appends:
#              "[TRUNCATED: result was N chars, showing first M
#                (budget: 30% of KK context window)]"
#      called right after every tool call completes:
#        crates/openfang-runtime/src/agent_loop.rs:1053  (and :2320, the
#        second loop variant)
#
#   3. The cut is *textually* disclosed inside the tool_result block (see
#      the marker above) — so it is not literally invisible on disk. But
#      in practice, on a real run, the model:
#        - correctly notices the marker in its private thinking
#          ("It says result was 117517 chars, showing first 78483 ...")
#        - correctly concludes there is no offset param to get the rest
#        - then STILL answers the caller's direct question "how many
#          characters did you get" with the file's TOTAL size (117517),
#          not the 78483 it actually received.
#      i.e. the disclosure exists in the raw tool output but does not
#      survive into the model's own summary — from the caller's point of
#      view this is indistinguishable from a silent truncation: the same
#      "success that wasn't" class of defect chased all through sprint 1,
#      just relocated one layer up the stack.
#
# This script reproduces the whole chain against a live staging agent:
# sends one real message asking it to report (a) the character count
# file_read returned and (b) the last 200 characters verbatim, then reads
# the resulting on-disk session .jsonl straight off the staging data volume
# to show what the tool actually returned underneath the model's answer,
# and compares both to the real file on disk (wc -c / tail -c 200).
#
# Usage: ./FANG-45.sh [base_url]      default: $OPENFANG_URL or staging
# Safe/idempotent: read-only against the daemon except for one /message
# call to an existing staging agent; touches no config, no cleanup needed.

set -uo pipefail

BASE_URL="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
CONFIG="${OF_CONFIG:-/var/lib/docker/volumes/openfang-staging-data/_data/config.toml}"
AGENT_NAME="${OF_AGENT_NAME:-AgentRAG2}"
REL_PATH="${OF_TX_PATH:-tx/7dbed435066d495fbaa3c7f1f57c35fb.txt}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production." >&2; exit 2;;
esac
for bin in curl python3; do
  command -v "$bin" >/dev/null || { echo "missing dependency: $bin" >&2; exit 3; }
done
[ -f "$CONFIG" ] || { echo "no config at $CONFIG" >&2; exit 3; }

API_KEY="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" | head -1)"
DATA_DIR="$(dirname "$CONFIG")"
WORKSPACE_DIR="$DATA_DIR/workspaces/$AGENT_NAME"
FILE_ON_DISK="$WORKSPACE_DIR/$REL_PATH"
SESSIONS_DIR="$WORKSPACE_DIR/sessions"

api() {
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -sS -m 620 -X "$m" -H 'Content-Type: application/json' \
         ${API_KEY:+-H "Authorization: Bearer $API_KEY"} -d "$b" "$BASE_URL$p"
  else
    curl -sS -m 620 -X "$m" ${API_KEY:+-H "Authorization: Bearer $API_KEY"} "$BASE_URL$p"
  fi
}

echo "=== FANG-45 · file_read silently truncates large files ==="
echo "date              : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "target            : $BASE_URL"
echo "daemon version    : $(api GET /api/health | python3 -c 'import sys,json;print(json.load(sys.stdin).get("version","?"))' 2>/dev/null)"
echo "agent             : $AGENT_NAME"
echo "file under test   : $REL_PATH"
echo

[ -f "$FILE_ON_DISK" ] || { echo "no such file on staging volume: $FILE_ON_DISK" >&2; exit 3; }

echo "--- 1. source check: file_read has no offset/limit param ---"
SCHEMA_LINES="$(grep -n -A9 'name: "file_read".to_string' "$REPO_ROOT/crates/openfang-runtime/src/tool_runner.rs" | head -10)"
echo "$SCHEMA_LINES"
if echo "$SCHEMA_LINES" | grep -qi 'offset\|limit\|range'; then
  echo "OFFSET/LIMIT PARAM: present"
else
  echo "OFFSET/LIMIT PARAM: ABSENT — file_read can only ever request the whole file"
fi
echo

echo "--- 2. real file on disk ---"
REAL_BYTES="$(wc -c < "$FILE_ON_DISK" | tr -d ' ')"
REAL_TAIL="$(tail -c 200 "$FILE_ON_DISK")"
echo "REAL_FILE_BYTES=$REAL_BYTES"
echo "REAL_LAST_200_BYTES (repr):"
python3 -c "import sys; print(repr(sys.argv[1]))" "$REAL_TAIL"
echo

echo "--- 3. resolve agent id for '$AGENT_NAME' ---"
AGENT_ID="$(api GET /api/agents | python3 -c "
import sys,json
agents=json.load(sys.stdin)
for a in agents:
    if a.get('name')=='$AGENT_NAME':
        print(a['id']); break
")"
[ -n "$AGENT_ID" ] || { echo "could not resolve agent id for $AGENT_NAME" >&2; exit 3; }
echo "AGENT_ID=$AGENT_ID"
echo

echo "--- 4. before: snapshot existing session files ---"
BEFORE_SESSIONS="$(ls "$SESSIONS_DIR" 2>/dev/null | sort)"
echo "$(echo "$BEFORE_SESSIONS" | wc -l) session file(s) present before"
echo

echo "--- 5. send the probe message (blocks for the full agent turn) ---"
PROMPT="Вызови file_read на пути $REL_PATH. Не пиши файл, не суммируй, не пересказывай. В ответе укажи ТОЛЬКО два факта дословно: (1) точное число символов, которое вернул тебе инструмент file_read (посчитай длину строки result дословно, как число), (2) последние 200 символов текста, которые он вернул, скопированные буквально посимвольно без изменений и без перевода."
BODY="$(python3 -c 'import json,sys; print(json.dumps({"message": sys.argv[1]}))' "$PROMPT")"
RESP="$(api POST "/api/agents/$AGENT_ID/message" "$BODY")"
echo "$RESP" | python3 -m json.tool 2>/dev/null || echo "$RESP"
echo

MODEL_ANSWER="$(echo "$RESP" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("response",""))' 2>/dev/null)"
echo "MODEL'S FINAL ANSWER TO THE CALLER:"
echo "$MODEL_ANSWER"
echo

echo "--- 6. after: find the new/updated session file ---"
AFTER_SESSIONS="$(ls "$SESSIONS_DIR" 2>/dev/null | sort)"
NEW_FILE="$(comm -13 <(echo "$BEFORE_SESSIONS") <(echo "$AFTER_SESSIONS") | head -1)"
if [ -z "$NEW_FILE" ]; then
  # no brand-new file: pick the most recently modified session (turn appended to it)
  NEW_FILE="$(ls -t "$SESSIONS_DIR" 2>/dev/null | head -1)"
fi
[ -n "$NEW_FILE" ] && [ -f "$SESSIONS_DIR/$NEW_FILE" ] || { echo "could not locate session file" >&2; exit 3; }
echo "session file: $SESSIONS_DIR/$NEW_FILE"
echo

echo "--- 7. what file_read actually returned underneath the model (from the raw session log) ---"
echo "NOTE: Rust's String::len() (used by content.len() in the truncation code and"
echo "quoted in the marker's 'N chars' text) is a BYTE count, not a Unicode codepoint"
echo "count. Python's len() on the JSON-decoded string counts codepoints instead, so"
echo "this section re-encodes to UTF-8 before measuring, to stay in the same units as"
echo "the marker and as wc -c above (all figures below are BYTES)."
python3 - "$SESSIONS_DIR/$NEW_FILE" "$REL_PATH" "$REAL_BYTES" <<'PY'
import json, sys
path, rel, real_bytes = sys.argv[1], sys.argv[2], int(sys.argv[3])
found = False
with open(path) as f:
    for line in f:
        try:
            obj = json.loads(line)
        except Exception:
            continue
        tu = obj.get("tool_use")
        if not tu:
            continue
        for call in tu:
            c = call.get("content")
            if not isinstance(c, str):
                continue
            if rel not in c and "TRUNCATED" not in c:
                continue
            found = True
            b = c.encode("utf-8")
            print(f"tool_result content length actually in prompt: {len(b)} bytes ({len(c)} unicode chars)")
            marker_start_char = c.find("[TRUNCATED:")
            if marker_start_char >= 0:
                marker_line = c[marker_start_char:].splitlines()[0]
                print("truncation marker found in raw tool_result:")
                print("  " + marker_line)
                kept_prefix = c[:marker_start_char].rstrip("\n")
                kept_bytes = len(kept_prefix.encode("utf-8"))
                pct = 100.0 * kept_bytes / real_bytes if real_bytes else 0
                print(f"  actual visible prefix before marker: {kept_bytes} bytes = {pct:.1f}% of the real {real_bytes}-byte file")
            else:
                print("truncation marker: ABSENT (tool_result was not truncated this call)")
                kept_prefix = c
            print("last 200 UTF-8 bytes actually delivered to the model, decoded for display (repr):")
            tail_bytes = kept_prefix.encode("utf-8")[-200:]
            print("  " + repr(tail_bytes.decode("utf-8", errors="replace")))
            print()
if not found:
    print("no matching tool_result block found for this file in the session log")
PY
echo

echo "--- 8. verdict ---"
echo "REAL_FILE_BYTES=$REAL_BYTES"
echo "MODEL_CLAIMED (from its final answer, first line) : $(echo "$MODEL_ANSWER" | head -1)"
echo
echo "See section 7 above for the actual chars delivered vs. the model's claim."
echo "If section 7 shows a [TRUNCATED: result was N chars, showing first M ...] marker"
echo "with M < N == REAL_FILE_BYTES, and the model's claimed count in section 8 does not"
echo "equal M, this is RED: the tool disclosed the cut in raw text, but (a) file_read has"
echo "no offset/limit to retrieve the missing tail (section 1), and (b) the model's"
echo "user-facing answer does not reliably carry the disclosure through — from the"
echo "caller's side this is indistinguishable from a silent partial read."
