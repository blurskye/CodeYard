#!/usr/bin/env bash
# One isolated round; scripts/e2e.sh owns fixture cleanup and the 25-minute deadline.
set -euo pipefail
unset E2E_UNDER_TIMEOUT
export E2E_DATABASES=pgbouncer E2E_TIMEOUT=1500
export E2E_PROFILE=${E2E_PROFILE:-debug}
exec bash scripts/e2e.sh
