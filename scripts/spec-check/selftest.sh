#!/usr/bin/env bash
# The checker must find exactly the four violations planted in fixtures/, and allow the rest.
set -euo pipefail
cd "$(dirname "$0")"
out=$(python3 check_logs.py --kube fixtures/audit-violations.log --s3 fixtures/garage-violations.log) && {
  echo "check_logs.py accepted logs with violations"; exit 1; }
echo "$out"
[ "$(grep -c 'NOT allowed by the spec: 2' <<<"$out")" = 2 ] || { echo "wrong verdicts"; exit 1; }
[ "$(grep -c 'allowed, \(different objects\|keys apart\): 1' <<<"$out")" = 2 ] || { echo "wrong verdicts"; exit 1; }
echo "selftest ok"
