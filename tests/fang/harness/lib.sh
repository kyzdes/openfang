#!/usr/bin/env bash
# FANG-52 harness — shared helpers for `fangrig`. Not meant to be run
# directly; sourced by tests/fang/harness/fangrig.
#
# See tests/fang/harness/README.md for the full picture. Short version:
# this file knows how to (a) refuse to touch production, (b) talk to the
# staging API, (c) reach into the staging container's network namespace to
# check ports and gate on stub reachability (python3 only — no curl/wget in
# the image, see F4), and (d) read/write tests/fang/harness state.

set -uo pipefail

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../../.." && pwd)"

BASE_URL="${OPENFANG_URL:-http://127.0.0.1:4201}"
CONTAINER="${OF_CONTAINER:-openfang-staging}"
CONFIG="${OPENFANG_CONFIG:-${OF_CONFIG:-/var/lib/docker/volumes/openfang-staging-data/_data/config.toml}}"
RUN_ROOT="${FANGRIG_RUN_ROOT:-/tmp/fang-harness}"
CURRENT_LINK="$RUN_ROOT/current"

PORT_MIN=8971
PORT_MAX=8979
TG_PORT_DEFAULT=8981
LLM_ROLE=fangrig
PROVIDER_KEY_ENV_DEFAULT="FANGRIG_API_KEY"

log()  { printf '%s\n' "$*" >&2; }
die()  { log "fangrig: $*"; exit "${2:-1}"; }

# ------------------------------------------------------------- prod guard --
guard_not_prod() {
  case "$BASE_URL" in
    *:4200*) die "REFUSING: BASE_URL=$BASE_URL looks like production (port 4200)." 2 ;;
  esac
  if [ "$CONTAINER" = "openfang-openfang-1" ]; then
    die "REFUSING: CONTAINER=$CONTAINER is the production container." 2
  fi
  if ! docker inspect "$CONTAINER" >/dev/null 2>&1; then
    die "REFUSING: container '$CONTAINER' not found — refusing to guess a target." 2
  fi
}

require_bin() {
  for b in "$@"; do
    command -v "$b" >/dev/null 2>&1 || die "missing dependency on host: $b" 3
  done
}

# --------------------------------------------------------------- api key --
_api_key_cache=""
api_key() {
  if [ -z "$_api_key_cache" ]; then
    _api_key_cache="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" 2>/dev/null | head -1)"
  fi
  printf '%s' "$_api_key_cache"
}

# api METHOD PATH [BODY] — talks to the staging OpenFang API.
api() {
  local m="$1" p="$2" b="${3:-}" k
  k="$(api_key)"
  if [ -n "$b" ]; then
    curl -sS -m 120 -X "$m" -H 'Content-Type: application/json' \
         ${k:+-H "Authorization: Bearer $k"} -d "$b" "$BASE_URL$p"
  else
    curl -sS -m 120 -X "$m" ${k:+-H "Authorization: Bearer $k"} "$BASE_URL$p"
  fi
}

# api_msg PATH BODY — like api(), but with the long timeout a real LLM turn
# needs (memory rule: -t 600 for /message, a 30s default reads as empty).
api_msg() {
  local p="$1" b="$2" k
  k="$(api_key)"
  curl -sS -m 600 -X POST -H 'Content-Type: application/json' \
       ${k:+-H "Authorization: Bearer $k"} -d "$b" "$BASE_URL$p"
}

# --------------------------------------------------------- container info --
container_image() { docker inspect -f '{{.Config.Image}}' "$CONTAINER"; }
container_pid()   { docker inspect -f '{{.State.Pid}}' "$CONTAINER"; }

# py_in_container CODE — runs python3 -c CODE inside $CONTAINER's own
# filesystem+netns (docker exec, not nsenter — no privilege needed for this
# and it stays inside the officially supported surface).
py_in_container() {
  docker exec "$CONTAINER" python3 -c "$1"
}

# port_free_in_container PORT — true if nothing is LISTENing on 127.0.0.1:PORT
# inside $CONTAINER's netns right now. Reads /proc/net/tcp(6) for state 0A
# (LISTEN) rather than trying to bind(): a plain bind() without
# SO_REUSEADDR gives a false "busy" for many seconds after the real
# listener is gone, because of TIME_WAIT sockets left over from ordinary
# closed HTTP connections (e.g. selftest's own probes) — those are not the
# same thing as something still listening.
port_free_in_container() {
  local port="$1"
  py_in_container "
import sys
port_hex = '%04X' % $port
found = False
for path in ('/proc/net/tcp', '/proc/net/tcp6'):
    try:
        lines = open(path).read().splitlines()[1:]
    except OSError:
        continue
    for line in lines:
        f = line.split()
        if len(f) < 4:
            continue
        local, state = f[1], f[3]
        lport = local.split(':')[-1]
        if lport == port_hex and state == '0A':  # 0A = LISTEN
            found = True
            break
    if found:
        break
sys.exit(1 if found else 0)
"
}

# health_gate PORT — the anti-false-green barrier from spec §7 step 6.
# Must be called with python3 (no curl/wget in the staging image, F4).
health_gate() {
  local port="$1" tries=10
  while [ "$tries" -gt 0 ]; do
    if py_in_container "
import urllib.request, sys
try:
    r = urllib.request.urlopen('http://127.0.0.1:$port/__health', timeout=2)
    sys.exit(0 if r.status == 200 else 1)
except Exception:
    sys.exit(1)
" ; then
      return 0
    fi
    tries=$((tries - 1))
    sleep 1
  done
  return 1
}

health_gate_fail_msg() {
  cat >&2 <<'MSG'
fangrig: stub unreachable from inside the staging container after 10s.

If you started the stub as a process on the HOST instead of a container in
the staging netns: traffic from the container to the host is dropped by
the firewall (a timeout, not a refusal — F2), and the OpenAI driver has no
.timeout() on its reqwest client (openai.rs:35-38 — F3). You will not see
an error. The turn will hang forever. That is the trap this harness exists
to avoid — always launch stubs with `fangrig up`, never by hand on the host.
MSG
}

# ------------------------------------------------------------- run state --
new_run_id() {
  printf '%s-%s' "$(date -u +%Y%m%d-%H%M%S)" "$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' \n')"
}

run_dir() {
  if [ -n "${1:-}" ]; then
    printf '%s/%s' "$RUN_ROOT" "$1"
  elif [ -L "$CURRENT_LINK" ]; then
    readlink -f "$CURRENT_LINK"
  else
    die "no current run — pass a run_id or run 'fangrig up' first" 3
  fi
}

state_file() { printf '%s/state.json' "$(run_dir "${1:-}")"; }

state_write() {
  # state_write RUN_DIR KEY=VALUE [KEY=VALUE ...] — VALUE is a raw JSON
  # literal (quote strings yourself: NAME='"str"').
  local dir="$1"; shift
  python3 - "$dir/state.json" "$@" <<'PY'
import json, sys
path = sys.argv[1]
try:
    d = json.load(open(path))
except Exception:
    d = {}
for kv in sys.argv[2:]:
    k, v = kv.split("=", 1)
    d[k] = json.loads(v)
json.dump(d, open(path, "w"), indent=2, sort_keys=True)
PY
}

# state_get DIR KEY — prints the value as a bare string if it's a JSON
# string, else as JSON (so callers can `[ "$(state_get ...)" = "true" ]`
# or pipe list/object results straight into `python3 -c 'json.loads(...)'`).
state_get() {
  python3 -c "
import json, sys
path, key = sys.argv[1], sys.argv[2]
try:
    d = json.load(open(path))
except Exception:
    d = {}
v = d.get(key)
print(v if isinstance(v, str) else json.dumps(v))
" "$1/state.json" "$2" 2>/dev/null
}

# ------------------------------------------------------- scenario helpers --
scenario_path() {
  local name="$1"
  local p="$HARNESS_DIR/scenarios/$name.json"
  [ -f "$p" ] || die "no such scenario: $name (looked in $HARNESS_DIR/scenarios/)" 3
  printf '%s' "$p"
}

# render_scenario SRC DST RUN_ID — copies a scenario JSON, stamping run_id.
# {{run_id}}/{{canary}} substitutions inside step content are done by the
# stubs themselves at request time (llm_stub.py / tg_stub.py); this just
# attaches the run_id so they know what canary to expect.
render_scenario() {
  local src="$1" dst="$2" run_id="$3"
  python3 -c "
import json
d = json.load(open('$src'))
d['run_id'] = '$run_id'
json.dump(d, open('$dst', 'w'), indent=2)
"
}

canary_for() { printf 'of-fangrig-canary-%s' "$1"; }

# stub_exec HOST PORT PATH METHOD [AUTH_BEARER] <<< BODY
# Runs an HTTP request against a stub from INSIDE $CONTAINER's own
# filesystem/netns — the only vantage point that can see it in --net share
# mode (F1), and body goes over stdin so we never have to shell-quote JSON
# through two layers of `-c`. Prints the status code on line 1, raw
# response body on the rest.
stub_exec() {
  local host="$1" port="$2" path="$3" method="$4" auth="${5:-}"
  docker exec -i "$CONTAINER" python3 -c "
import sys, urllib.request, urllib.error
body = sys.stdin.buffer.read()
req = urllib.request.Request('http://$host:$port$path', data=(body or None), method='$method')
req.add_header('Content-Type', 'application/json')
$( [ -n "$auth" ] && printf "req.add_header('Authorization', 'Bearer %s')\n" "$auth" )
try:
    r = urllib.request.urlopen(req, timeout=15)
    sys.stdout.write(str(r.status) + chr(10))
    sys.stdout.flush()
    sys.stdout.buffer.write(r.read())
except urllib.error.HTTPError as e:
    sys.stdout.write(str(e.code) + chr(10))
    sys.stdout.flush()
    sys.stdout.buffer.write(e.read())
except Exception as e:
    sys.stdout.write('ERR' + chr(10))
    sys.stdout.write(str(e))
"
}

# stub_host: which hostname reaches the LLM stub from inside $CONTAINER,
# given the run's net_mode.
stub_host() {
  local dir="$1"
  local mode
  mode="$(state_get "$dir" net_mode)"
  if [ "$mode" = "bridge" ]; then printf 'fangrig-llm'; else printf '127.0.0.1'; fi
}

tg_stub_host() {
  local dir="$1"
  local mode
  mode="$(state_get "$dir" net_mode)"
  if [ "$mode" = "bridge" ]; then printf 'fangrig-tg'; else printf '127.0.0.1'; fi
}
