#!/usr/bin/env bash
# A-4 — '## Current Date' (and other service prompt sections) leak into agent-authored markdown.
#
# Mechanism (source of truth, NOT docs):
#   crates/openfang-runtime/src/prompt_builder.rs:82-84
#       if let Some(ref date) = ctx.current_date {
#           sections.push(format!("## Current Date\nToday is {date}."));
#       }
#   The whole system prompt is `sections.join("\n\n")` where every section is a
#   markdown "## Heading" block (see SERVICE_PATTERNS below). An agent asked to
#   author a markdown document can copy any of them into its output.
#
# What this script measures: of 10 independent "write a markdown file" tasks,
# how many produced a file containing a service section.
#
# Usage:  ./A-4.sh [BASE_URL]        (default: $OPENFANG_URL, else http://127.0.0.1:4201)
#         ./A-4.sh --scan-only [BASE_URL]   re-scan existing artifacts + the whole
#                                           staging corpus, no LLM calls (~1 s)
# Idempotent: wipes its own out/test-a4-*.md before each run, resets the session
# between every task, and touches nothing but the agent's own out/ directory.
#
# STAGING ONLY. Never point this at 127.0.0.1:4200 (prod).

set -uo pipefail

SCAN_ONLY=0
if [ "${1:-}" = "--scan-only" ]; then SCAN_ONLY=1; shift; fi

BASE_URL="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
AGENT_NAME="${A4_AGENT:-AgentGemma4}"
N_TASKS="${A4_TASKS:-10}"
SKILL_SCRIPTS="${SKILL_SCRIPTS:-/root/.claude/skills/openfang/scripts}"
STAGING_DATA="${OPENFANG_HOME_HOST:-/var/lib/docker/volumes/openfang-staging-data/_data}"
export PATH="$SKILL_SCRIPTS:$PATH"
export OPENFANG_URL="$BASE_URL"
export OPENFANG_CONFIG="${OPENFANG_CONFIG:-$STAGING_DATA/config.toml}"

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. Staging is :4201." >&2; exit 2;;
esac

# Service section headings emitted by prompt_builder.rs / injected verbatim from
# the agent's AGENTS.md. Any of these appearing in a deliverable is a leak.
SERVICE_PATTERNS=(
  '## Current Date'
  'Today is '
  '## Tool Call Behavior'
  '## Your Tools'
  '## Memory'
  '## Skills'
  '## Connected Tool Servers'
  '## Workspace'
  '## Identity'
  '## Persona'
  '## User Context'
  '## Long-Term Memory'
  '## User Profile'
  '## Channel'
  '## Sender'
  '## Peer Agents'
  '## Safety'
  '## Operational Guidelines'
  '## Heartbeat Checklist'
  '## First-Run Protocol'
  '## Live Context'
)

# A heading pattern only counts at the START of a line, otherwise "### Skills vs
# Agents" in an ordinary document is a false positive. Body patterns (no leading
# "## ") are matched anywhere.
match_pattern() { # $1 = pattern, $2 = file
  case "$1" in
    '## '*) grep -qE "^$1" "$2" ;;
    *)      grep -qF -- "$1" "$2" ;;
  esac
}
list_matching() { # $1 = pattern, $2 = root
  case "$1" in
    '## '*) grep -rlE --include='*.md' -- "^$1" "$2" 2>/dev/null ;;
    *)      grep -rlF --include='*.md' -- "$1" "$2" 2>/dev/null ;;
  esac
}

TOPICS=(
  "how a bicycle derailleur works"
  "the life cycle of a honeybee colony"
  "types of knots used in sailing"
  "how lighthouses were built on rock"
  "brewing green tea properly"
  "desert plants and how they store water"
  "the parts of an acoustic guitar"
  "harvesting rainwater at a small house"
  "how paper is made from wood pulp"
  "the stages of making sourdough bread"
)

echo "=== A-4 baseline run"
echo "date_utc:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "base_url:   $BASE_URL"
echo "version:    $(ofctl -x version GET /api/health 2>/dev/null)"
echo "agent:      $AGENT_NAME"
echo "tasks:      $N_TASKS"
echo

AGENT_ID=$(curl -s "$BASE_URL/api/agents" | python3 -c '
import sys,json
name=sys.argv[1]
for a in json.load(sys.stdin):
    if a["name"]==name:
        print(a["id"]); break
' "$AGENT_NAME")
if [ -z "$AGENT_ID" ]; then echo "FATAL: agent $AGENT_NAME not found on $BASE_URL" >&2; exit 1; fi
MODEL=$(curl -s "$BASE_URL/api/agents" | python3 -c '
import sys,json
name=sys.argv[1]
for a in json.load(sys.stdin):
    if a["name"]==name:
        print(a["model_provider"]+"/"+a["model_name"]); break
' "$AGENT_NAME")
echo "agent_id:   $AGENT_ID"
echo "model:      $MODEL"

WS_HOST="$STAGING_DATA/workspaces/$AGENT_NAME"
OUT_HOST="$WS_HOST/out"
echo "out_dir:    $OUT_HOST"
echo

# --- idempotence: clear previous run's artifacts (own test files only) -------
mkdir -p "$OUT_HOST"
if [ "$SCAN_ONLY" = "0" ]; then rm -f "$OUT_HOST"/test-a4-*.md; fi

# --- prove the deployed binary still carries the unpatched format ------------
CONTAINER="${OPENFANG_CONTAINER:-openfang-staging}"
BINHITS=$(docker exec "$CONTAINER" grep -ac '## Current Date' /usr/local/bin/openfang 2>/dev/null || echo "?")
echo "binary '## Current Date' literal present in $CONTAINER:/usr/local/bin/openfang : $BINHITS"
echo

# --- run the 10 independent tasks -------------------------------------------
if [ "$SCAN_ONLY" = "1" ]; then echo "(--scan-only: skipping LLM tasks)"; fi
for i in $(seq 1 "$N_TASKS"); do
  if [ "$SCAN_ONLY" = "1" ]; then break; fi
  topic="${TOPICS[$((i-1))]}"
  echo "--- task $i/$N_TASKS : $topic"
  ofctl POST "/api/agents/$AGENT_ID/session/reset" >/dev/null 2>&1 || true
  msg="Write a markdown file at out/test-a4-$i.md about $topic. Give it 3-4 sections. Use the file_write tool."
  body=$(python3 -c 'import json,sys;print(json.dumps({"message":sys.argv[1]}))' "$msg")
  # -t 600 is mandatory: /message blocks for the whole agent loop; ofctl's 30s
  # default returns an empty body while the agent keeps running server-side.
  resp=$(ofctl -t 600 POST "/api/agents/$AGENT_ID/message" "$body" 2>&1)
  echo "$resp" | head -c 400
  echo
  if [ -f "$OUT_HOST/test-a4-$i.md" ]; then
    echo "    file: PRESENT ($(wc -c <"$OUT_HOST/test-a4-$i.md") bytes)"
  else
    echo "    file: MISSING"
  fi
done

echo
echo "=== on-disk scan ($OUT_HOST) — report is based on FILES, never on agent replies"
LEAK=0; PRESENT=0
for i in $(seq 1 "$N_TASKS"); do
  f="$OUT_HOST/test-a4-$i.md"
  if [ ! -f "$f" ]; then echo "test-a4-$i.md : NO FILE"; continue; fi
  PRESENT=$((PRESENT+1))
  hits=""
  for p in "${SERVICE_PATTERNS[@]}"; do
    if match_pattern "$p" "$f"; then hits="$hits [$p]"; fi
  done
  if [ -n "$hits" ]; then
    LEAK=$((LEAK+1))
    echo "test-a4-$i.md : LEAK ->$hits"
    grep -nF -- '## Current Date' "$f" | sed 's/^/      /'
    grep -nF -- 'Today is ' "$f" | sed 's/^/      /'
  else
    echo "test-a4-$i.md : clean"
  fi
done

DATEONLY=0
for i in $(seq 1 "$N_TASKS"); do
  f="$OUT_HOST/test-a4-$i.md"
  [ -f "$f" ] || continue
  if match_pattern '## Current Date' "$f" || match_pattern 'Today is ' "$f"; then
    DATEONLY=$((DATEONLY+1))
  fi
done

echo
echo "=== METRIC (prescribed 10-task protocol, $AGENT_NAME / $MODEL)"
echo "files_written:                  $PRESENT / $N_TASKS"
echo "files_with_ANY_service_section: $LEAK / $N_TASKS"
echo "files_with_DATE_section:        $DATEONLY / $N_TASKS"

# ---------------------------------------------------------------------------
# Secondary evidence: does the leak exist anywhere in the staging corpus that
# this script did NOT author? This is observational, not a controlled trial —
# those documents were produced by other agents/models over the last days.
# ---------------------------------------------------------------------------
echo
echo "=== corpus-wide scan of $STAGING_DATA (*.md, excluding this script's test-a4-* files"
echo "    and excluding the per-agent context files that legitimately hold headings)"
EXCL='/(AGENTS|SOUL|IDENTITY|USER|MEMORY|BOOTSTRAP|HEARTBEAT|TOOLS)\.md$'
CORPUS_TOTAL=$(find "$STAGING_DATA" -name '*.md' 2>/dev/null | grep -v 'test-a4-' | grep -vE "$EXCL" | wc -l)
echo "corpus_md_files: $CORPUS_TOTAL"
echo "NOTE: hits still need human classification — a file under hands/ is authored"
echo "      documentation, not agent output, and a topic like 'Skills' can be organic."
for p in "${SERVICE_PATTERNS[@]}"; do
  files=$(list_matching "$p" "$STAGING_DATA" | grep -v 'test-a4-' | grep -vE "$EXCL")
  n=$(printf '%s' "$files" | grep -c . )
  printf '%3s  %s\n' "$n" "$p"
  if [ "$n" != "0" ]; then
    printf '%s\n' "$files" | sed 's/^/       -> /'
  fi
done

echo
echo "=== distinct leaked documents in the corpus (deduplicated by content hash)"
find "$STAGING_DATA" -name '*.md' 2>/dev/null | grep -v 'test-a4-' | grep -vE "$EXCL" \
  | while read -r f; do
      if match_pattern '## Current Date' "$f" || match_pattern 'Today is ' "$f"; then
        echo "$(md5sum "$f" | cut -c1-12)  $(stat -c %y "$f" | cut -c1-19)  $f"
      fi
    done | sort

echo
echo "=== all service '## ' sections assembled into the system prompt (source of truth)"
cat <<'EOF'
crates/openfang-runtime/src/prompt_builder.rs — build_system_prompt() joins these with "\n\n":
   :84   ## Current Date              <- the reported defect; body is "Today is {date}."
   :143  ## Heartbeat Checklist       (autonomous agents only)
   :195  ## First-Run Protocol
   :219  ## Live Context
   :245  ## Tool Call Behavior        (const TOOL_CALL_BEHAVIOR)
   :274  ## Your Tools
   :311  ## Memory                    (build_memory_section — injected a SECOND time by
                                       agent_loop.rs:396 and :1624, so it can appear twice)
   :337  ## Skills
   :352  ## Connected Tool Servers (MCP)
   :365  ## Workspace
   :371  ## Identity
   :379  ## Persona
   :387  ## User Context
   :393  ## Long-Term Memory
   :404  ## User Profile              (also :409 for the unknown-name branch)
   :449  ## Channel
   :457  ## Sender                    (also :458, :459)
   :466  ## Peer Agents
   :484  ## Safety                    (const SAFETY_SECTION)
   :493  ## Operational Guidelines    (const OPERATIONAL_GUIDELINES)
crates/openfang-runtime/src/workspace_context.rs
   :130  ## Workspace Context         (+ "### <filename>" subsections at :153)

Injected VERBATIM with no wrapper heading — the file's own markdown headings become
top-level prompt structure:
   prompt_builder.rs:99   AGENTS.md          (this box: "# Agent Behavioral Guidelines",
                                              "## Core Principles", "## Tool Usage Protocols",
                                              "## Response Style")
   prompt_builder.rs:205  workspace_context
   prompt_builder.rs:232  base_system_prompt (build_identity_section)

=> A fix must change the FORMAT OF SERVICE SECTIONS AS A CLASS (e.g. a non-markdown
   delimiter, or an XML-ish envelope), not just the date line at :84.
EOF
echo
echo "=== date format that produced the leak"
echo 'kernel.rs:2268 and kernel.rs:2851 (two call sites, identical):'
echo '    current_date: Some(chrono::Local::now()'
echo '        .format("%A, %B %d, %Y (%Y-%m-%d %H:%M %Z)").to_string())'
echo 'so the leaked line looks like: "Today is Tuesday, August 11, 2026 (2026-08-11 22:29 +00:00)."'
