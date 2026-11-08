#!/usr/bin/env bash
# One isolated round; scripts/e2e.sh owns fixture cleanup and the 25-minute deadline.
set -euo pipefail
unset E2E_UNDER_TIMEOUT
export E2E_DATABASES=cells E2E_TIMEOUT=1500
export E2E_PROFILE=${E2E_PROFILE:-debug}
# Match the integration tests' stack budget for unoptimized async server code.
export RUST_MIN_STACK=${RUST_MIN_STACK:-33554432}
exec bash scripts/e2e.sh
