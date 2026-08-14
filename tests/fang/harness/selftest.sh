#!/usr/bin/env bash
# Thin wrapper so `tests/fang/harness/selftest.sh` works standalone, per the
# FANG-52 spec's file layout. The real implementation is `cmd_selftest` in
# ./fangrig — this just forwards to it (`fangrig selftest [run_id]`).
set -uo pipefail
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/fangrig" selftest "$@"
