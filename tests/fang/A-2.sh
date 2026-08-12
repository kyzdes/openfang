#!/usr/bin/env bash
# A-2: remove_custom_model does not recompute provider.model_count
#
# Deterministic, no LLM. Idempotent: creates its own test- model, deletes it,
# and cleans up on exit. Never touches existing custom models.
#
# Usage: A-2.sh [base_url]        (default: $OPENFANG_URL, else http://127.0.0.1:4201)
#
# Exit 0 = defect REPRODUCED (counter drift observed after DELETE)
# Exit 1 = no drift (defect absent / patched)
set -uo pipefail

BASE="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
export OPENFANG_URL="$BASE"
export PATH="/root/.claude/skills/openfang/scripts:$PATH"

PROVIDER="${A2_PROVIDER:-hyperfusion}"
MODEL_ID="${A2_MODEL_ID:-test-a2/drift-probe}"

say() { printf '%s\n' "$*"; }

# model_count as reported by /api/providers for $PROVIDER
counter() {
  ofctl GET /api/providers | python3 -c "
import json,sys
d=json.load(sys.stdin)
p=[x for x in d['providers'] if x['id']=='$PROVIDER']
print(p[0]['model_count'] if p else 'NA')
"
}

# actual number of models of $PROVIDER in /api/models
factual() {
  ofctl GET /api/models | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(sum(1 for m in d['models'] if m['provider']=='$PROVIDER'))
"
}

cleanup() {
  # best-effort removal of our probe model (wildcard route: slash goes literally)
  ofctl DELETE "/api/models/custom/${MODEL_ID}" >/dev/null 2>&1
}

say "== A-2 baseline probe =="
say "date:     $(date -Is)"
say "base_url: $BASE"
say "version:  $(ofctl -x version GET /api/health)"
say "provider: $PROVIDER"
say "probe id: $MODEL_ID"
say ""

cleanup   # start from a clean slate (idempotency)

C0=$(counter); F0=$(factual)
say "step 0 (initial):        providers.model_count=$C0  actual=/api/models=$F0  drift=$((C0-F0))"

say ""
say "step 1: POST /api/models/custom"
ofctl POST /api/models/custom "{\"id\":\"$MODEL_ID\",\"provider\":\"$PROVIDER\",\"display_name\":\"A-2 drift probe\"}"
C1=$(counter); F1=$(factual)
say "step 1 (after create):   providers.model_count=$C1  actual=$F1  drift=$((C1-F1))"

say ""
say "step 2: DELETE /api/models/custom/$MODEL_ID"
ofctl DELETE "/api/models/custom/${MODEL_ID}"
C2=$(counter); F2=$(factual)
say "step 2 (after delete):   providers.model_count=$C2  actual=$F2  drift=$((C2-F2))"

DRIFT=$((C2-F2))
say ""
say "RESULT: drift after delete = $DRIFT model(s)"
say "  expected if defect present: drift=1 (counter stuck at $C1, actual back to $F0)"
say "  expected if fixed:          drift=0"

trap cleanup EXIT
if [ "$DRIFT" -ne 0 ]; then
  say "VERDICT: RED — remove_custom_model did not recompute model_count"
  exit 0
else
  say "VERDICT: GREEN — counter consistent"
  exit 1
fi
