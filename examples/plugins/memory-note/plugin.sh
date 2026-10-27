#!/bin/sh
# `memory` writes to shared memory after the call, scoped to the session. This
# is how a fact learned by one harness becomes visible to all of them.
event="$1"
payload=$(cat)
[ "$event" = "post_call" ] || { echo '{}'; exit 0; }

# No jq dependency: the harness name is enough for this example.
harness=$(printf '%s' "$payload" | sed -n 's/.*"harness":"\([^"]*\)".*/\1/p' | head -1)
printf '{"memory":{"last_harness":"%s"}}\n' "$harness"
