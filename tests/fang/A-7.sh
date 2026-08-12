#!/usr/bin/env bash
# A-7 — a manifest [[fallback_models]] entry without an explicit base_url does not
#        resolve its own provider's URL from [provider_urls]; it inherits
#        [default_model].base_url instead, and the request goes to the wrong host.
#
# Mechanism (source of truth, NOT docs):
#
#   crates/openfang-kernel/src/kernel.rs:5729-5733 — per-agent manifest fallback:
#       base_url: fb
#           .base_url
#           .clone()
#           .or_else(|| dm.base_url.clone())          <-- THE BUG: default_model wins
#           .or_else(|| self.lookup_provider_url(&fb_provider)),
#
#   Compare the PRIMARY model, 100 lines above (kernel.rs:5619-5630), which gets
#   it right and says so in a comment:
#       // Don't inherit default provider's base_url when switching providers.
#       let base_url = if has_custom_url { manifest.model.base_url.clone() }
#           else if agent_provider == default_provider {
#               effective_default.base_url.clone()
#                   .or_else(|| self.lookup_provider_url(agent_provider))
#           } else { self.lookup_provider_url(agent_provider) };
#
#   And compare the GLOBAL [[fallback_providers]] chain (kernel.rs:5771-5774),
#   which also gets it right:
#       base_url: fb.base_url.clone().or_else(|| self.lookup_provider_url(&fb.provider)),
#
#   So [provider_urls] IS consulted for a manifest fallback -- but only third, after
#   dm.base_url has already claimed the slot. The defect therefore fires exactly when
#   the fallback's provider differs from [default_model].provider AND
#   [default_model].base_url is set. The fallback driver is then built with provider
#   B's API key pointed at provider A's host => 401.
#
# Four agents, so that cause is separable from coincidence. All four have the same
# message sent to them; every primary model that is meant to fail is pointed at
# http://127.0.0.1:9/v1 (discard port, instant connection refused, costs nothing).
#
#   test-a7-inherit     dead primary + fallback y7router/kimi/k3, NO base_url
#                       -> EXPECT FAILURE: driver built on [default_model].base_url
#                          (hyperfusion) carrying the y7router key => 401
#   test-a7-explicit    same, but the fallback carries base_url explicitly
#                       -> EXPECT SUCCESS. Control for "bad key / dead model".
#   test-a7-control     y7router/kimi/k3 as the PRIMARY model, no base_url anywhere,
#                       no fallback
#                       -> EXPECT SUCCESS. Proves the primary path does inherit
#                          [provider_urls], i.e. the asymmetry is real, and proves
#                          the y7router key + kimi/k3 are healthy.
#   test-a7-inherit-hf  dead primary + fallback hyperfusion/gemma-4, NO base_url
#                       -> EXPECT SUCCESS. The discriminating case: the fallback
#                          provider equals [default_model].provider, so the wrong
#                          inheritance happens to yield the right URL. If this one
#                          passes while test-a7-inherit fails, the cause is the
#                          dm.base_url hijack and not "fallbacks never get a URL".
#
# Metric: the HTTP status of test-a7-inherit vs test-a7-explicit.
#         Red baseline = inherit fails with 401 in the body, explicit succeeds.
#
# Usage:  ./A-7.sh [BASE_URL]        (default: $OPENFANG_URL, else http://127.0.0.1:4201)
# Idempotent: uninstalls its four test-a7-* agents (registry + on-disk dir) before
# and after each run. Touches nothing else. Never writes to config.toml.
#
# STAGING ONLY. Never point this at 127.0.0.1:4200 (prod).

set -uo pipefail

BASE_URL="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
SKILL_SCRIPTS="${SKILL_SCRIPTS:-/root/.claude/skills/openfang/scripts}"
STAGING_DATA="${OPENFANG_HOME_HOST:-/var/lib/docker/volumes/openfang-staging-data/_data}"
CONTAINER="${OPENFANG_CONTAINER:-openfang-staging}"
DEAD_URL="${A7_DEAD_URL:-http://127.0.0.1:9/v1}"
MSG="${A7_MSG:-Reply with exactly: PONG}"
TIMEOUT="${A7_TIMEOUT:-600}"

export PATH="$SKILL_SCRIPTS:$PATH"
export OPENFANG_URL="$BASE_URL"
export OPENFANG_CONFIG="${OPENFANG_CONFIG:-$STAGING_DATA/config.toml}"

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. Staging is :4201." >&2; exit 2;;
esac

# ---------------------------------------------------------------- credentials
# Read the daemon key the same way ofctl does: top-level api_key only, stop at the
# first [table]. `grep '^api_key'` also matches api_key_env and yields a 71-char
# concatenation that 400s with an empty body.
API_KEY="$(awk '/^\[/{exit} /^api_key[[:space:]]*=/{gsub(/^[^"]*"|"[^"]*$/,""); print; exit}' \
  "$OPENFANG_CONFIG")"
if [ -z "${API_KEY:-}" ]; then
  echo "FATAL: no top-level api_key in $OPENFANG_CONFIG" >&2; exit 2
fi
CURLRC="$(mktemp)"; chmod 600 "$CURLRC"
printf 'header = "Authorization: Bearer %s"\n' "$API_KEY" > "$CURLRC"
trap 'rm -f "$CURLRC"' EXIT

api() { # api METHOD PATH [BODY] -> body on stdout, "\nHTTP:<code>" last line
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -sS -K "$CURLRC" -m "$TIMEOUT" -X "$m" \
      -H 'Content-Type: application/json' -d "$b" \
      -w '\nHTTP:%{http_code}' "$BASE_URL$p"
  else
    curl -sS -K "$CURLRC" -m "$TIMEOUT" -X "$m" -w '\nHTTP:%{http_code}' "$BASE_URL$p"
  fi
}

AGENTS="test-a7-inherit test-a7-explicit test-a7-control test-a7-inherit-hf"

cleanup_agents() {
  local list ids
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
    # DELETE /uninstall removes $HOME/agents/<name>/, but POST /api/agents never
    # wrote one (spawn is registry-only), so the only trace left is the agent's
    # state dir under workspaces/. Remove it so a re-run starts from zero.
    # Guarded: the name must be one of ours and the path must sit directly under
    # the workspaces root.
    case "$n" in
      test-a7-*)
        ws="$STAGING_DATA/workspaces/$n"
        if [ -d "$ws" ] && [ "$(dirname "$ws")" = "$STAGING_DATA/workspaces" ]; then
          rm -rf -- "$ws" && echo "  removed workspace $ws"
        fi
        ;;
    esac
  done
}

# ------------------------------------------------------------------ manifests
# A manifest whose ONLY difference between cases is the fallback base_url line.
manifest() { # manifest NAME PRIMARY_PROVIDER PRIMARY_MODEL PRIMARY_BASEURL FALLBACK_BLOCK
  cat <<EOF
name = "$1"
version = "1.0.0"
description = "A-7 baseline probe"
author = "fang-tests"
module = "builtin:chat"
schedule = "reactive"
priority = "Normal"
tags = ["test-a7"]

[model]
provider = "$2"
model = "$3"
max_tokens = 256
temperature = 0.0
system_prompt = "You are a probe. Answer in one word."
api_key_env = "$(echo "$2" | tr 'a-z-' 'A-Z_')_API_KEY"
$( [ -n "$4" ] && echo "base_url = \"$4\"" )
$5
EOF
}

FB_NO_URL='
[[fallback_models]]
provider = "y7router"
model = "kimi/k3"
api_key_env = "Y7ROUTER_API_KEY"
# base_url deliberately absent -- this is the defect under test'

FB_WITH_URL='
[[fallback_models]]
provider = "y7router"
model = "kimi/k3"
api_key_env = "Y7ROUTER_API_KEY"
base_url = "https://router.y7.hk/v1"'

FB_HF_NO_URL='
[[fallback_models]]
provider = "hyperfusion"
model = "google/gemma-4-31b-it"
api_key_env = "HYPERFUSION_API_KEY"
# base_url absent, but this provider IS [default_model].provider'

# ----------------------------------------------------------------------- run
echo "=================================================================="
echo "A-7 baseline — FallbackModel base_url resolution"
echo "date         : $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "base url     : $BASE_URL"
printf 'image ver    : '; ofctl -x version GET /api/health 2>/dev/null || echo '?'
echo "container    : $CONTAINER ($(docker inspect -f '{{.Config.Image}}' "$CONTAINER" 2>/dev/null))"
echo "dead primary : $DEAD_URL"
echo
echo "--- relevant config.toml (the bug needs default_model.base_url set) ---"
sed -n '/^\[default_model\]/,/^$/p;/^\[provider_urls\]/,/^$/p' "$OPENFANG_CONFIG" \
  | sed 's/\(api_key_env *= *"\).*"/\1<REDACTED>"/'
echo

echo "--- cleanup (idempotency) ---"
cleanup_agents
echo

declare -A CODE BODY
for case_name in $AGENTS; do
  case "$case_name" in
    test-a7-inherit)    m="$(manifest "$case_name" hyperfusion google/gemma-4-31b-it "$DEAD_URL" "$FB_NO_URL")";;
    test-a7-explicit)   m="$(manifest "$case_name" hyperfusion google/gemma-4-31b-it "$DEAD_URL" "$FB_WITH_URL")";;
    test-a7-control)    m="$(manifest "$case_name" y7router kimi/k3 "" "")";;
    test-a7-inherit-hf) m="$(manifest "$case_name" hyperfusion google/gemma-4-31b-it "$DEAD_URL" "$FB_HF_NO_URL")";;
  esac

  echo "=== $case_name ==="
  echo "--- manifest ---"
  printf '%s\n' "$m" | sed 's/\(api_key_env *= *"\).*"/\1<REDACTED>"/'

  body="$(printf '%s' "$m" | python3 -c 'import json,sys; print(json.dumps({"manifest_toml": sys.stdin.read()}))')"
  out="$(api POST /api/agents "$body")"
  spawn_code="${out##*HTTP:}"
  aid="$(printf '%s' "${out%$'\n'HTTP:*}" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("agent_id") or json.load(sys.stdin).get("id") or "")
except Exception: pass' 2>/dev/null)"
  if [ -z "$aid" ]; then
    aid="$(printf '%s' "${out%$'\n'HTTP:*}" | python3 -c 'import json,sys,re
s=sys.stdin.read()
m=re.search(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",s)
print(m.group(0) if m else "")')"
  fi
  echo "--- spawn: HTTP $spawn_code  agent_id=${aid:-<none>} ---"
  if [ -z "$aid" ]; then
    echo "SPAWN FAILED: ${out%$'\n'HTTP:*}"
    CODE[$case_name]="spawn-failed"; BODY[$case_name]="${out%$'\n'HTTP:*}"
    echo; continue
  fi

  # Prove the manifest reached disk (an agent report is not evidence).
  if [ -f "$STAGING_DATA/agents/$case_name/agent.toml" ]; then
    echo "on-disk manifest: $STAGING_DATA/agents/$case_name/agent.toml"
    echo "  fallback base_url on disk: $(grep -c 'base_url' "$STAGING_DATA/agents/$case_name/agent.toml") base_url line(s)"
  else
    echo "on-disk manifest: ABSENT (registry only)"
  fi

  # Exact log window: docker logs --since has 1s granularity, which bleeds the
  # previous case's lines into this one. Count lines instead.
  log_before="$(docker logs "$CONTAINER" 2>&1 | wc -l)"
  echo "--- POST /api/agents/$aid/message (timeout ${TIMEOUT}s) ---"
  t0=$(date +%s)
  out="$(api POST "/api/agents/$aid/message" \
    "$(python3 -c 'import json,sys; print(json.dumps({"message": sys.argv[1]}))' "$MSG")")"
  t1=$(date +%s)
  code="${out##*HTTP:}"; resp="${out%$'\n'HTTP:*}"
  CODE[$case_name]="$code"; BODY[$case_name]="$resp"
  echo "HTTP $code   ($((t1-t0))s elapsed)"
  printf '%s\n' "$resp" | head -c 2000; echo

  echo "--- daemon log for this call (lines added during it) ---"
  docker logs "$CONTAINER" 2>&1 \
    | tail -n "+$((log_before + 1))" \
    | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -iE 'fallback|401|driver|refused|y7router|hyperfusion' \
    | head -25
  echo
done

echo "=================================================================="
echo "RESULT TABLE"
printf '%-20s %-8s %s\n' AGENT HTTP VERDICT
for n in $AGENTS; do
  c="${CODE[$n]:-?}"
  b="${BODY[$n]:-}"
  if printf '%s' "$b" | grep -q '"response"'; then v="answered"; else v="no response field"; fi
  if printf '%s' "$b" | grep -qE '401|Authentication Error|token_not_found'; then v="$v / 401 IN BODY"; fi
  printf '%-20s %-8s %s\n' "$n" "$c" "$v"
done
echo
echo "METRIC: inherit=${CODE[test-a7-inherit]:-?}  explicit=${CODE[test-a7-explicit]:-?}"
echo "        control=${CODE[test-a7-control]:-?}  inherit-hf=${CODE[test-a7-inherit-hf]:-?}"
echo "RED baseline means: inherit != 2xx (401 in body) AND explicit == 200"
echo "                    AND control == 200 AND inherit-hf == 200"
echo

echo "--- cleanup ---"
cleanup_agents
echo "done."
