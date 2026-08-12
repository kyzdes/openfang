#!/usr/bin/env bash
# A-6 — a silent model substitution: when the primary model fails and the
#        FallbackDriver serves the turn from a different model, the response body
#        of POST /api/agents/{id}/message contains NOTHING that identifies which
#        model actually produced the text. Second half of the defect: the one log
#        line that does record the switch prints an EMPTY model name for the
#        primary (driver_index=0 model=).
#
# Mechanism (source of truth, NOT docs):
#
#   1) The response type has no room for it.
#      crates/openfang-api/src/types.rs:55-62
#          pub struct MessageResponse {
#              pub response: String,
#              pub input_tokens: u64,
#              pub output_tokens: u64,
#              pub iterations: u32,
#              #[serde(skip_serializing_if = "Option::is_none")]
#              pub cost_usd: Option<f64>,
#          }
#      Five fields, none of them naming a model or provider. The handler that
#      fills it (routes.rs:417-426) has nothing else to put there.
#
#   2) The information is destroyed one layer down, before the API could see it.
#      crates/openfang-runtime/src/llm_driver.rs:72-81 — CompletionResponse carries
#      content / stop_reason / tool_calls / usage. No model field. So when
#      FallbackDriver returns the winning driver's response
#      (drivers/fallback.rs:47  `Ok(response) => return Ok(response)`) the identity
#      of the winner is discarded at that return. AgentLoopResult
#      (agent_loop.rs:249-259) never had it either.
#
#   3) The log line that does fire cannot name the primary.
#      crates/openfang-kernel/src/kernel.rs:5693-5696 pushes the primary with a
#      deliberately EMPTY model name:
#          let mut chain: ... = vec![(primary.clone(), String::new())];
#      with the comment "Primary driver uses an empty model name so the request's
#      `model` field ... is used as-is". drivers/fallback.rs:57-63 then logs that
#      same empty string:
#          Err(e) => { warn!(driver_index = i, model = %model_name,
#                            error = %e, "Fallback driver failed, trying next") }
#      `model_name` is "" for i=0, so the operator is told that *something*
#      failed but not what. The real name is sitting in `req.model`
#      (fallback.rs:42-45) and is not logged.
#
#   4) Metering actively MISATTRIBUTES the call to the model that failed.
#      crates/openfang-kernel/src/kernel.rs:2998-3012
#          let model = &manifest.model.model;          // the PRIMARY, always
#          ... self.metering.record(&UsageRecord { model: model.clone(), ... })
#      The manifest primary is recorded no matter which driver served the turn, so
#      GET /api/usage/by-model reports the tokens under the dead model. This is
#      worse than silence: the one API surface that aggregates per model states
#      something false. Same for GET /api/agents/{id} (routes.rs:1503-1506), which
#      reports `entry.manifest.model.model` — configuration, not fact.
#
# Design. Two agents differing ONLY in whether the primary works, so that
# "fallback served the turn" is the single variable and the response shapes can be
# compared key for key:
#
#   test-a6-fallback  primary hyperfusion/openai/gpt-oss-120b at a DEAD base_url
#                     (http://127.0.0.1:9/v1 — discard port, refused instantly and
#                     free), fallback hyperfusion/google/gemma-4-31b-it with an
#                     EXPLICIT base_url.
#                     -> every turn is served by gemma-4. 10 turns.
#   test-a6-control   byte-identical manifest except the primary base_url is the
#                     real one, so the primary (gpt-oss-120b) serves.
#                     -> the discriminator. If the two response bodies carry the
#                        same key set, the API cannot express the difference, and
#                        no client can detect a substitution.
#
# The fallback's base_url is EXPLICIT on purpose: without it this would also trip
# A-7 (a manifest fallback inherits [default_model].base_url instead of resolving
# [provider_urls]) and the two defects would be entangled. Both agents also use
# ONE provider, so provider-key resolution is identical in both arms.
#
# Prompts are neutral arithmetic. The agent is never asked "which model are you" as
# part of the metric: a model's self-report is not the API disclosing anything, and
# models routinely get their own identity wrong. That question is asked once, after
# the metric is closed, and labelled as a side probe.
#
# METRIC: of 10 fallback-served responses, how many let the caller determine the
#         model that actually wrote the text, using only the HTTP response
#         (headers + body). Expected red baseline: 0 of 10.
#
# Usage:  ./A-6.sh [BASE_URL]        (default: $OPENFANG_URL, else http://127.0.0.1:4201)
# Idempotent: removes its own test-a6-* agents (registry + workspace) before and
# after each run. Writes nothing outside them. Never edits config.toml.
#
# STAGING ONLY. Never point this at 127.0.0.1:4200 (prod).

set -uo pipefail

BASE_URL="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
SKILL_SCRIPTS="${SKILL_SCRIPTS:-/root/.claude/skills/openfang/scripts}"
STAGING_DATA="${OPENFANG_HOME_HOST:-/var/lib/docker/volumes/openfang-staging-data/_data}"
CONTAINER="${OPENFANG_CONTAINER:-openfang-staging}"
DEAD_URL="${A6_DEAD_URL:-http://127.0.0.1:9/v1}"
LIVE_URL="${A6_LIVE_URL:-https://api.hyperfusion.io/v1}"
PRIMARY_MODEL="${A6_PRIMARY_MODEL:-openai/gpt-oss-120b}"
FALLBACK_MODEL="${A6_FALLBACK_MODEL:-google/gemma-4-31b-it}"
N="${A6_N:-10}"
TIMEOUT="${A6_TIMEOUT:-600}"

export PATH="$SKILL_SCRIPTS:$PATH"
export OPENFANG_URL="$BASE_URL"
export OPENFANG_CONFIG="${OPENFANG_CONFIG:-$STAGING_DATA/config.toml}"

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. Staging is :4201." >&2; exit 2;;
esac

# ---------------------------------------------------------------- credentials
# Read the daemon key the way ofctl does: top-level api_key only, stop at the first
# [table]. A bare `grep '^api_key'` also matches api_key_env and yields a 71-char
# concatenation that 400s with an empty body.
API_KEY="$(awk '/^\[/{exit} /^api_key[[:space:]]*=/{gsub(/^[^"]*"|"[^"]*$/,""); print; exit}' \
  "$OPENFANG_CONFIG")"
if [ -z "${API_KEY:-}" ]; then
  echo "FATAL: no top-level api_key in $OPENFANG_CONFIG" >&2; exit 2
fi
CURLRC="$(mktemp)"; chmod 600 "$CURLRC"
printf 'header = "Authorization: Bearer %s"\n' "$API_KEY" > "$CURLRC"
HDR="$(mktemp)"
trap 'rm -f "$CURLRC" "$HDR"' EXIT

api() { # api METHOD PATH [BODY] -> body, then a final line "HTTP:<code>"
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -sS -K "$CURLRC" -m "$TIMEOUT" -X "$m" -D "$HDR" \
      -H 'Content-Type: application/json' -d "$b" \
      -w '\nHTTP:%{http_code}' "$BASE_URL$p"
  else
    curl -sS -K "$CURLRC" -m "$TIMEOUT" -X "$m" -D "$HDR" \
      -w '\nHTTP:%{http_code}' "$BASE_URL$p"
  fi
}

AGENTS="test-a6-fallback test-a6-control"

cleanup_agents() {
  local list ids ws
  list="$(curl -sS -K "$CURLRC" -m 30 "$BASE_URL/api/agents")" || return 0
  for n in $AGENTS; do
    ids="$(printf '%s' "$list" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit()
print(' '.join(a['id'] for a in d if a.get('name')=='$n'))")"
    for id in $ids; do
      curl -sS -K "$CURLRC" -m 60 -X DELETE "$BASE_URL/api/agents/$id/uninstall" >/dev/null
      echo "  cleaned $n ($id)"
    done
    # POST /api/agents is registry-only and writes no agents/<name>/ dir, so the
    # only trace left is the name-derived state dir under workspaces/. Guarded:
    # the name must be one of ours and the parent must be the workspaces root.
    case "$n" in
      test-a6-*)
        ws="$STAGING_DATA/workspaces/$n"
        if [ -d "$ws" ] && [ "$(dirname "$ws")" = "$STAGING_DATA/workspaces" ]; then
          rm -rf -- "$ws" && echo "  removed workspace $ws"
        fi
        ;;
    esac
  done
}

# ------------------------------------------------------------------- manifests
# Identical but for the primary base_url. api_key_env is the same in both arms, so
# credential resolution cannot explain any difference.
manifest() { # manifest NAME PRIMARY_BASEURL
  cat <<EOF
name = "$1"
version = "1.0.0"
description = "A-6 baseline probe: silent model substitution"
author = "fang-tests"
module = "builtin:chat"
schedule = "reactive"
priority = "Normal"
tags = ["test-a6"]

[model]
provider = "hyperfusion"
model = "$PRIMARY_MODEL"
max_tokens = 128
temperature = 0.0
system_prompt = "You are a calculator. Answer with the number only, nothing else."
api_key_env = "HYPERFUSION_API_KEY"
base_url = "$2"

[[fallback_models]]
provider = "hyperfusion"
model = "$FALLBACK_MODEL"
api_key_env = "HYPERFUSION_API_KEY"
base_url = "$LIVE_URL"
EOF
}

spawn() { # spawn NAME PRIMARY_BASEURL -> prints agent id
  local m body out aid
  m="$(manifest "$1" "$2")"
  body="$(printf '%s' "$m" | python3 -c 'import json,sys; print(json.dumps({"manifest_toml": sys.stdin.read()}))')"
  out="$(api POST /api/agents "$body")"
  aid="$(printf '%s' "${out%$'\n'HTTP:*}" | python3 -c 'import json,sys,re
s=sys.stdin.read()
try:
    d=json.loads(s); print(d.get("agent_id") or d.get("id") or ""); raise SystemExit
except SystemExit: raise
except Exception: pass
m=re.search(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",s)
print(m.group(0) if m else "")')"
  printf '%s' "$aid"
}

# The determinability judgement, kept in one place so the rule is auditable.
# A response counts as DETERMINABLE if either
#   (a) it carries a field whose name claims to identify the model/provider/
#       substitution, or
#   (b) the actual model's distinctive name appears anywhere in body or headers.
#
# The program lives in a temp file rather than a heredoc: `python3 - <<PY` would
# take its *source* from stdin, so a body piped in on stdin is silently discarded
# and every response gets judged as an empty string — a vacuous "NO". The body is
# therefore passed as argv[4].
JUDGE_PY="$(mktemp --suffix=.py)"
trap 'rm -f "$CURLRC" "$HDR" "$JUDGE_PY"' EXIT
cat > "$JUDGE_PY" <<'PY'
import json, sys, re
hdrfile, primary, fallback = sys.argv[1], sys.argv[2], sys.argv[3]
raw = sys.argv[4]
try:
    hdr = open(hdrfile, encoding='utf-8', errors='replace').read()
except Exception:
    hdr = ''
try:
    d = json.loads(raw)
except Exception:
    d = None
keys = sorted(d.keys()) if isinstance(d, dict) else []
IDENT = {'model', 'model_used', 'provider', 'provider_used', 'fallback',
         'fallback_used', 'driver', 'driver_index', 'engine', 'served_by'}
ident_keys = [k for k in keys if k.lower() in IDENT]
# distinctive token of each model name: the part after the last '/'
def tok(m): return m.rsplit('/', 1)[-1].split('-')[0].lower()
pt, ft = tok(primary), tok(fallback)
hay = (raw + '\n' + hdr).lower()
named = sorted({t for t in (pt, ft) if t and t in hay})
determinable = bool(ident_keys) or bool(named)
print("KEYS        : " + (','.join(keys) if keys else '<not a json object>'))
print("IDENT_KEYS  : " + (','.join(ident_keys) if ident_keys else '<none>'))
print("MODEL_NAMED : " + (','.join(named) if named else '<none>'))
print("DETERMINABLE: " + ("YES" if determinable else "NO"))
PY

judge() { # judge HEADER_FILE PRIMARY FALLBACK BODY
  python3 "$JUDGE_PY" "$1" "$2" "$3" "$4"
}

# Self-test the judge before trusting a single verdict: a hand-made body that DOES
# name the model must come back YES, and the real 4-key shape must come back NO.
# Without this, a broken judge reports a clean 0-of-N red for the wrong reason.
judge_selftest() {
  local st_pos st_neg
  echo "--- judge self-test (guards against a vacuous red) ---"
  st_pos="$(judge /dev/null "$PRIMARY_MODEL" "$FALLBACK_MODEL" \
    '{"response":"5","model_used":"google/gemma-4-31b-it"}' | grep DETERMINABLE)"
  st_neg="$(judge /dev/null "$PRIMARY_MODEL" "$FALLBACK_MODEL" \
    '{"response":"5","input_tokens":1,"output_tokens":1,"iterations":1}' | grep DETERMINABLE)"
  echo "  body naming the model     -> $st_pos  (must be YES)"
  echo "  body with the 4 real keys -> $st_neg  (must be NO)"
  case "$st_pos|$st_neg" in
    *YES*\|*NO*) echo "  judge OK";;
    *) echo "  JUDGE BROKEN — verdicts would be meaningless. Aborting." >&2; exit 3;;
  esac
}

# ------------------------------------------------------------------------ run
echo "=================================================================="
echo "A-6 baseline — silent model substitution by FallbackDriver"
echo "date          : $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "base url      : $BASE_URL"
printf 'image version : '; ofctl -x version GET /api/health 2>/dev/null || echo '?'
echo "container     : $CONTAINER ($(docker inspect -f '{{.Config.Image}}' "$CONTAINER" 2>/dev/null))"
echo "image id      : $(docker inspect -f '{{.Image}}' "$CONTAINER" 2>/dev/null)"
echo "primary model : $PRIMARY_MODEL"
echo "fallback model: $FALLBACK_MODEL"
echo "dead base_url : $DEAD_URL   (primary of test-a6-fallback)"
echo "live base_url : $LIVE_URL"
echo "turns         : $N"
echo
echo "--- config.toml, relevant tables (secrets redacted) ---"
sed -n '/^\[default_model\]/,/^$/p;/^\[provider_urls\]/,/^$/p' "$OPENFANG_CONFIG" \
  | sed 's/\(api_key[a-z_]* *= *"\)[^"]*"/\1<REDACTED>"/'
echo
echo "--- global [[fallback_providers]] in config.toml? ---"
if grep -q '^\[\[fallback_providers\]\]' "$OPENFANG_CONFIG"; then
  echo "PRESENT — the chain has extra entries beyond the manifest; note it."
  sed -n '/^\[\[fallback_providers\]\]/,/^$/p' "$OPENFANG_CONFIG" \
    | sed 's/\(api_key[a-z_]* *= *"\)[^"]*"/\1<REDACTED>"/'
else
  echo "absent — the chain is exactly [primary, manifest fallback], length 2."
fi
echo

judge_selftest
echo

echo "--- cleanup (idempotency) ---"
cleanup_agents
echo

echo "--- usage attributed by model BEFORE the run ---"
BY_MODEL_BEFORE="$(api GET /api/usage/by-model)"
printf '%s\n' "$BY_MODEL_BEFORE" | head -40
echo; echo

echo "--- spawning ---"
FB_ID="$(spawn test-a6-fallback "$DEAD_URL")"
CT_ID="$(spawn test-a6-control  "$LIVE_URL")"
echo "test-a6-fallback = ${FB_ID:-<FAILED>}"
echo "test-a6-control  = ${CT_ID:-<FAILED>}"
if [ -z "$FB_ID" ] || [ -z "$CT_ID" ]; then
  echo "FATAL: spawn failed, cannot measure." >&2; cleanup_agents; exit 1
fi
echo

# What the pre-existing API surfaces claim about the agent, before any traffic.
echo "--- GET /api/agents/\$FB_ID  (does any field name a model?) ---"
api "GET" "/api/agents/$FB_ID" | python3 -c 'import json,sys
s=sys.stdin.read()
s=s.rsplit("HTTP:",1)[0]
try:
    d=json.loads(s)
    print("top-level keys:", ",".join(sorted(d.keys())))
    print("model field   :", json.dumps(d.get("model")))
except Exception as e:
    print("unparsed:", s[:400])'
echo

PROMPTS=(
  "What is 2 plus 3?"
  "What is 7 minus 4?"
  "What is 6 times 6?"
  "What is 20 divided by 5?"
  "What is 11 plus 12?"
  "What is 9 times 3?"
  "What is 100 minus 58?"
  "What is 8 plus 8?"
  "What is 45 divided by 9?"
  "What is 13 plus 4?"
)

DET=0; ANSWERED=0
LOG_START="$(docker logs "$CONTAINER" 2>&1 | wc -l)"

for i in $(seq 1 "$N"); do
  p="${PROMPTS[$(( (i-1) % ${#PROMPTS[@]} ))]}"
  echo "=================== turn $i/$N ==================="
  echo "prompt: $p"
  # Independent turns: wipe history first so turn i cannot inherit turn i-1.
  api POST "/api/agents/$FB_ID/session/reset" >/dev/null 2>&1
  before="$(docker logs "$CONTAINER" 2>&1 | wc -l)"
  t0=$(date +%s)
  out="$(api POST "/api/agents/$FB_ID/message" \
    "$(python3 -c 'import json,sys; print(json.dumps({"message": sys.argv[1]}))' "$p")")"
  t1=$(date +%s)
  code="${out##*HTTP:}"; resp="${out%$'\n'HTTP:*}"
  echo "HTTP $code   ($((t1-t0))s)"
  echo "--- response headers ---"
  sed 's/\r$//' "$HDR" | sed '/^$/d'
  echo "--- response body (verbatim) ---"
  printf '%s\n' "$resp"
  echo "--- determinability ---"
  jout="$(judge "$HDR" "$PRIMARY_MODEL" "$FALLBACK_MODEL" "$resp")"
  printf '%s\n' "$jout"
  printf '%s' "$resp" | grep -q '"response"' && ANSWERED=$((ANSWERED+1))
  printf '%s\n' "$jout" | grep -q 'DETERMINABLE: YES' && DET=$((DET+1))
  echo "--- daemon log lines added by this turn ---"
  docker logs "$CONTAINER" 2>&1 | tail -n "+$((before + 1))" \
    | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -iE 'fallback|driver_index|refused|classified|hyperfusion' | head -12
  echo
done

echo "=================================================================="
echo "CONTROL — same manifest, LIVE primary, so the primary serves"
api POST "/api/agents/$CT_ID/session/reset" >/dev/null 2>&1
ctl_before="$(docker logs "$CONTAINER" 2>&1 | wc -l)"
out="$(api POST "/api/agents/$CT_ID/message" '{"message":"What is 2 plus 3?"}')"
ctl_code="${out##*HTTP:}"; ctl_resp="${out%$'\n'HTTP:*}"
echo "HTTP $ctl_code"
echo "--- response headers ---"; sed 's/\r$//' "$HDR" | sed '/^$/d'
echo "--- response body (verbatim) ---"; printf '%s\n' "$ctl_resp"
echo "--- determinability ---"
judge "$HDR" "$PRIMARY_MODEL" "$FALLBACK_MODEL" "$ctl_resp"
echo "--- daemon log for the control turn (expect NO fallback line) ---"
docker logs "$CONTAINER" 2>&1 | tail -n "+$((ctl_before + 1))" \
  | sed 's/\x1b\[[0-9;]*m//g' | grep -iE 'fallback|driver_index' | head -8
echo
echo "--- KEY-SET COMPARISON: fallback-served vs primary-served ---"
python3 - "$resp" "$ctl_resp" <<'PY'
import json, sys
def keys(s):
    try: return sorted(json.loads(s).keys())
    except Exception: return ['<unparsed>']
a, b = keys(sys.argv[1]), keys(sys.argv[2])
print("fallback-served keys:", ",".join(a))
print("primary-served  keys:", ",".join(b))
print("IDENTICAL KEY SETS  :", "YES" if a == b else "NO")
print("=> a client comparing response shape",
      "CANNOT" if a == b else "CAN",
      "tell a substitution happened")
PY
echo

echo "=================================================================="
echo "SECOND HALF OF THE DEFECT — is model= empty at driver_index=0?"
echo "--- all fallback/driver_index lines emitted during this run ---"
docker logs "$CONTAINER" 2>&1 | tail -n "+$((LOG_START + 1))" \
  | sed 's/\x1b\[[0-9;]*m//g' | grep -iE 'fallback|driver_index'
echo
docker logs "$CONTAINER" 2>&1 | tail -n "+$((LOG_START + 1))" \
  | sed 's/\x1b\[[0-9;]*m//g' | grep -E 'driver_index=0' \
  | python3 -c 'import sys,re
lines=[l.rstrip("\n") for l in sys.stdin]
print("driver_index=0 lines:", len(lines))
empty=0; named=0
for l in lines:
    m=re.search(r"model=(\S*)", l)
    if m is None: continue
    if m.group(1)=="" or m.group(1) in ("", '"'"''"'"'): empty+=1
    else: named+=1
# a bare "model=" followed by whitespace means the field rendered empty
empty=sum(1 for l in lines if re.search(r"model=(\s|$)", l))
named=len(lines)-empty
print("  with EMPTY model= :", empty)
print("  with a model name :", named)
print("VERDICT:", "empty model= confirmed at driver_index=0" if empty and not named
      else ("model name IS logged — retrospective is wrong" if named and not empty
            else "mixed/none — inspect the raw lines above"))'
echo

echo "=================================================================="
echo "IS THERE ANY OTHER API ROUTE THAT REVEALS THE ACTUAL MODEL?"
for route in \
  "/api/agents/$FB_ID" \
  "/api/agents/$FB_ID/session" \
  "/api/agents/$FB_ID/history" \
  "/api/agents/$FB_ID/config" \
  "/api/sessions" \
  "/api/usage" \
  "/api/usage/by-model" \
  "/api/usage/summary" \
  "/api/metrics" \
  "/api/health/detail" \
  "/api/audit/recent" ; do
  echo "--- GET $route ---"
  api GET "$route" 2>&1 | head -c 1200
  echo; echo
done

echo "--- MISATTRIBUTION: /api/usage/by-model call_count delta over this run ---"
echo "kernel.rs:2998-3012 records manifest.model.model (the PRIMARY) for every turn,"
echo "whichever driver actually served it. If that is what happens, the $N fallback"
echo "turns land under $PRIMARY_MODEL — a model that answered nothing — and"
echo "$FALLBACK_MODEL, which answered everything, gains only the control agent's share."
BY_MODEL_AFTER="$(api GET /api/usage/by-model)"
python3 - "$BY_MODEL_BEFORE" "$BY_MODEL_AFTER" "$PRIMARY_MODEL" "$FALLBACK_MODEL" "$N" <<'PY'
import json, sys
def load(s):
    s = s.rsplit('HTTP:', 1)[0]
    try:
        return {m['model']: m for m in json.loads(s).get('models', [])}
    except Exception:
        return {}
before, after = load(sys.argv[1]), load(sys.argv[2])
primary, fallback, n = sys.argv[3], sys.argv[4], int(sys.argv[5])
names = sorted(set(before) | set(after))
print(f"  {'model':45s} {'calls before':>12s} {'calls after':>11s} {'delta':>7s}")
for m in names:
    b = before.get(m, {}).get('call_count', 0)
    a = after.get(m, {}).get('call_count', 0)
    mark = ''
    if m == primary:
        mark = '   <- dead primary, answered NOTHING'
    elif m == fallback:
        mark = '   <- actually produced every fallback answer'
    print(f"  {m:45s} {b:12d} {a:11d} {a-b:7d}{mark}")
dp = after.get(primary, {}).get('call_count', 0) - before.get(primary, {}).get('call_count', 0)
df = after.get(fallback, {}).get('call_count', 0) - before.get(fallback, {}).get('call_count', 0)
print()
print(f"  delta[{primary}]  = {dp}   (expected {n}+1 if misattributed: {n} fallback turns + 1 control turn)")
print(f"  delta[{fallback}] = {df}   (expected 0 if misattributed, despite serving {n} turns)")
print("  MISATTRIBUTED:", "YES" if dp > df else "NO")
PY
echo
echo "--- /api/metrics labels for the two probe agents ---"
echo "openfang_tokens_total carries provider= and model= labels. They come from the"
echo "manifest too, so check whether test-a6-fallback is labelled with the dead model."
api GET /api/metrics | grep -E 'test-a6' || echo "  (no test-a6 lines — metrics window is hourly/rolling)"
echo

echo "=================================================================="
echo "SIDE PROBE (NOT part of the metric) — ask the model to name itself."
echo "A self-report is the model talking, not the API disclosing; models are"
echo "routinely wrong about their own identity. Recorded for completeness only."
api POST "/api/agents/$FB_ID/session/reset" >/dev/null 2>&1
api POST "/api/agents/$FB_ID/message" \
  '{"message":"Which model are you? Reply with just the model identifier."}' | head -c 800
echo
cat <<'EOT'
  How to read this: if the text above happens to name the fallback model, that is
  the model volunteering its identity, not the runtime reporting it. It does not
  close the defect. It costs an extra turn, it cannot be asked retroactively about
  a document already written, it is unverifiable (nothing cross-checks the claim),
  and other models get this wrong or refuse. Treat it as a curiosity, not a
  workaround.
EOT
echo

echo "=================================================================="
echo "DISCLOSURE CHANNELS — interpretation of the raw sections above."
echo "Re-read the raw output after any patch; only the first row is computed."
printf '  POST /api/agents/{id}/message   %d of %d turns named the model -> %s\n' \
  "$DET" "$N" "$([ "$DET" -eq 0 ] && echo SILENT || echo DISCLOSES)"
cat <<'EOT'
  GET  /api/agents/{id}           "model" = manifest primary        -> WRONG
  GET  /api/agents/{id}/session   roles + text only                -> SILENT
  GET  /api/usage/by-model        keyed on manifest primary         -> WRONG
  GET  /api/metrics               model= label from manifest        -> WRONG
  GET  /api/health/detail         no per-call data                  -> SILENT
  GET  /api/audit/recent          no LLM-call entries               -> SILENT
  docker logs (daemon WARN)       records the switch, but model= is
                                  empty for the primary            -> PARTIAL
  Note the distinction that matters for severity: the routes marked WRONG do not
  merely omit the fact, they assert the configured model as though it were the one
  that ran. A dashboard or a cost report built on them is confidently incorrect.
EOT
echo
echo "RESULT"
echo "turns sent                        : $N"
echo "turns that returned a response    : $ANSWERED"
echo "turns naming the ACTUAL model     : $DET"
echo
echo "METRIC (A-6 before): $DET of $N responses disclose the model that wrote them"
echo "RED baseline means : 0 of $N, with the log showing a fallback per turn"
echo "                     and an empty model= at driver_index=0"
echo

echo "--- cleanup ---"
cleanup_agents
echo "done."
