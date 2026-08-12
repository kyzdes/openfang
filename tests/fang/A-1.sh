#!/bin/bash
# A-1: PUT /api/agents/{id}/update is a no-op stub that returns HTTP 200.
#
# Source of the defect (openfang v0.6.9, ветка ours):
#   crates/openfang-api/src/routes.rs  fn update_agent  -- parses into `_manifest`,
#     drops it, replies 200 {"status":"acknowledged"}.
#   crates/openfang-api/src/types.rs   AgentUpdateRequest { manifest_toml: String }
#   docs/api-reference.md "PUT /api/agents/{id}/update" documents a completely
#     different body {description, system_prompt, tags} and a {"status":"updated"} reply.
#
# Red criteria on the unpatched build:
#   1. PUT with a valid manifest_toml whose fields are CHANGED -> HTTP 200
#      "acknowledged", and nothing changes: not GET /api/agents/{id}, not agent.toml.
#   2. The body from the docs -> HTTP 4xx (missing field `manifest_toml`).
#   Control: PATCH /api/agents/{id} {"description":...} -> 200 AND really applies,
#   so a non-effect on PUT is the endpoint's fault, not the probe's.
#
# Green after the patch: either PUT really applies the manifest, or it stops
# reporting success (and the docs match the implemented contract).
#
# Idempotent: recreates test-a1-probe from scratch on every run.
# Usage: A-1.sh [BASE_URL]     default $OPENFANG_URL or http://127.0.0.1:4201 (СТЕНД)

set -u
export PATH=/root/.claude/skills/openfang/scripts:$PATH
BASE=${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}
export OPENFANG_URL=$BASE
export OPENFANG_CONFIG=${OPENFANG_CONFIG:-/var/lib/docker/volumes/openfang-staging-data/_data/config.toml}
VOL=${OPENFANG_VOLUME:-/var/lib/docker/volumes/openfang-staging-data/_data}
NAME=test-a1-probe
DISK="$VOL/agents/$NAME/agent.toml"

jqf() { python3 -c 'import sys,json;d=json.load(sys.stdin);print(json.dumps({k:d.get(k) for k in ("id","name","description","system_prompt","model","tags")},ensure_ascii=False,sort_keys=True))'; }
field() { python3 -c 'import sys,json;print(json.load(sys.stdin).get("'"$1"'"))'; }

echo "=== A-1 baseline probe: PUT /api/agents/{id}/update ==="
echo "date:          $(date -Is)"
echo "url:           $BASE"
echo "image version: $(ofctl -x version GET /api/health 2>/dev/null)"
echo

# ---------- cleanup previous probe (idempotency) ----------
for old in $(ofctl -r GET /api/agents 2>/dev/null | python3 -c '
import sys,json
d=json.load(sys.stdin)
for a in (d if isinstance(d,list) else d.get("agents",[])):
    if a.get("name")=="'"$NAME"'": print(a["id"])' 2>/dev/null); do
    echo "-- deleting stale probe $old -> HTTP $(ofctl -s DELETE "/api/agents/$old" 2>&1)"
done
rm -rf "$VOL/agents/$NAME"

mk_manifest() { # $1 description  $2 max_tokens  $3 system_prompt
cat <<EOF
name = "$NAME"
version = "0.1.0"
description = "$1"
author = ""
module = "builtin:chat"
schedule = "reactive"
tags = []

[model]
provider = "hyperfusion"
model = "openai/gpt-oss-120b"
max_tokens = $2
temperature = 0.7
system_prompt = "$3"
api_key_env = "HYPERFUSION_API_KEY"

[capabilities]
network = []
tools = []
memory_read = []
memory_write = []
EOF
}
wrap() { python3 -c 'import json,sys;print(json.dumps({"manifest_toml":sys.stdin.read()}))'; }

BEFORE_DESC="A1-BEFORE-description"; AFTER_DESC="A1-AFTER-description"
BEFORE_SP="A1-BEFORE-prompt";        AFTER_SP="A1-AFTER-prompt"

# ---------- 1. create the probe ----------
echo "--- POST /api/agents (create probe, description=$BEFORE_DESC max_tokens=4096) ---"
CREATE=$(mk_manifest "$BEFORE_DESC" 4096 "$BEFORE_SP" | wrap | ofctl -r POST /api/agents -)
echo "$CREATE"
AID=$(printf '%s' "$CREATE" | field agent_id)
echo "agent_id: $AID"
echo

echo "--- observable BEFORE ---"
ofctl -r GET "/api/agents/$AID" | jqf
echo "disk $DISK:"
if [ -f "$DISK" ]; then
    grep -E '^description|^ *max_tokens|^ *system_prompt' "$DISK"
    BEFORE_MD5=$(md5sum "$DISK" | cut -d' ' -f1); echo "md5: $BEFORE_MD5"
else
    echo "  <absent>"; BEFORE_MD5=absent
fi
echo

# ---------- 2. PUT /update with a CHANGED manifest ----------
mk_manifest "$AFTER_DESC" 1234 "$AFTER_SP" | wrap > /tmp/a1-put.json
echo "--- PUT /api/agents/\$AID/update  (description->$AFTER_DESC, max_tokens->1234, system_prompt->$AFTER_SP) ---"
PUT_STATUS=$(ofctl -s PUT "/api/agents/$AID/update" @/tmp/a1-put.json 2>&1)
echo "HTTP status: $PUT_STATUS"
echo -n "body: "; ofctl -r PUT "/api/agents/$AID/update" @/tmp/a1-put.json 2>&1; echo
echo

# ---------- 3. observable AFTER ----------
echo "--- observable AFTER ---"
AFTER_JSON=$(ofctl -r GET "/api/agents/$AID")
printf '%s' "$AFTER_JSON" | jqf
API_DESC=$(printf '%s' "$AFTER_JSON" | field description)
API_SP=$(printf '%s' "$AFTER_JSON" | field system_prompt)
echo "disk $DISK:"
if [ -f "$DISK" ]; then
    grep -E '^description|^ *max_tokens|^ *system_prompt' "$DISK"
    AFTER_MD5=$(md5sum "$DISK" | cut -d' ' -f1); echo "md5: $AFTER_MD5"
else
    echo "  <absent>"; AFTER_MD5=absent
fi
[ "$BEFORE_MD5" = "$AFTER_MD5" ] && DISK_VERDICT=UNCHANGED || DISK_VERDICT=CHANGED
echo "disk verdict: $DISK_VERDICT"
if [ "$API_DESC" = "$AFTER_DESC" ] && [ "$API_SP" = "$AFTER_SP" ]; then APPLIED=yes; else APPLIED=no; fi
echo "description via API: $API_DESC   (expected if applied: $AFTER_DESC)"
echo "system_prompt via API: $API_SP   (expected if applied: $AFTER_SP)"
echo "APPLIED: $APPLIED"
echo

# ---------- 4. the DOCUMENTED body ----------
DOCBODY='{"description":"Updated description","system_prompt":"You are a specialized assistant.","tags":["updated","v2"]}'
echo "--- PUT /update with the body documented in docs/api-reference.md ---"
echo "request: $DOCBODY"
DOC_STATUS=$(ofctl -s PUT "/api/agents/$AID/update" "$DOCBODY" 2>&1)
echo "HTTP status: $DOC_STATUS"
echo -n "body: "; ofctl -r PUT "/api/agents/$AID/update" "$DOCBODY" 2>/dev/null; echo
echo

# ---------- 5. control: PATCH, which is implemented ----------
echo "--- control: PATCH /api/agents/\$AID {\"description\":\"A1-PATCH-works\"} ---"
echo "HTTP status: $(ofctl -s PATCH "/api/agents/$AID" '{"description":"A1-PATCH-works"}' 2>&1)"
PATCH_DESC=$(ofctl -r GET "/api/agents/$AID" | field description)
echo "description after PATCH: $PATCH_DESC"
[ "$PATCH_DESC" = "A1-PATCH-works" ] && PATCH_APPLIED=yes || PATCH_APPLIED=no
echo "PATCH APPLIED: $PATCH_APPLIED"
echo "disk $DISK after PATCH:"
if [ -f "$DISK" ]; then grep -E '^description|^ *max_tokens|^ *system_prompt' "$DISK"; PATCH_DISK=present; else echo "  <absent>"; PATCH_DISK=absent; fi
echo "  (note: agent.toml is written only by PATCH's persist_manifest_to_disk;"
echo "   POST /api/agents and PUT /update never touched it -- max_tokens is still 4096,"
echo "   not the 1234 the PUT manifest asked for.)"
echo

echo "=== METRIC (baseline, unpatched) ==="
echo "PUT /update, valid manifest_toml     -> HTTP $PUT_STATUS, applied=$APPLIED, disk=$DISK_VERDICT"
echo "PUT /update, documented body         -> HTTP $DOC_STATUS"
echo "PATCH /api/agents/{id} (control)     -> applied=$PATCH_APPLIED"
echo
echo "RED if: PUT status is 200 AND applied=no AND disk=UNCHANGED AND control applied=yes."
echo "probe agent left in place: $AID / $NAME"
