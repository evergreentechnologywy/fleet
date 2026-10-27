#!/bin/sh
# A guard is the clearest use of `skip`: it runs before the call, sees which
# harness and model were chosen, and can refuse.
event="$1"
payload=$(cat)
[ "$event" = "pre_call" ] || { echo '{}'; exit 0; }

state="${FLEET_HOME:-$HOME/.fleet}/budget.count"
day=$(date -u +%Y-%m-%d)
count=$(cat "$state" 2>/dev/null || echo "0 $day")
set -- $count
n=$1; d=$2
[ "$d" = "$day" ] || n=0
LIMIT=${FLEET_DAILY_LIMIT:-500}
n=$((n + 1))
printf '%s %s' "$n" "$day" > "$state"

if [ "$n" -gt "$LIMIT" ]; then
  printf '{"skip":true,"skip_reason":"daily limit %s reached"}\n' "$LIMIT"
else
  printf '{"note":"call %s of %s today"}\n' "$n" "$LIMIT"
fi
