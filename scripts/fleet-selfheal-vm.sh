#!/bin/sh
# The fleet's out-of-band self-healer.
#
# WHY A VM AND NOT A LOCAL TIMER: a local `fleet-doctor.timer` cannot heal a host
# that is down, wedged, or has lost its own disk — the healer would be as dead as
# the patient. A Maritime machine is somewhere else, so it survives the hub.
#
# WHAT IT DOES, every 5 minutes:
#   1. reads the hub's /health over Tailscale;
#   2. if the hub is up, asks it for its own doctor verdict and reports drift;
#   3. if the hub is DOWN, it says so loudly — that is the failure a local timer
#      can never report about itself;
#   4. writes its findings into the fleet's shared memory through the hub when the
#      hub is reachable, and into its own log when it is not.
#
# WHAT IT DELIBERATELY DOES NOT DO: restart services or edit config on the hub.
# A remote process that can rewrite the hub is a bigger risk than the outage it
# heals, and the hub already heals itself locally (fleet-doctor.timer). This is a
# WATCHER with a memory, not a remote root shell.
#
# Install on a Maritime machine (they ship the `hermes` template, which has sh):
#   curl -fsSL <this-file> -o /usr/local/bin/fleet-selfheal && chmod +x ...
#   echo '*/5 * * * * /usr/local/bin/fleet-selfheal >> /var/log/fleet-selfheal.log 2>&1' | crontab -

set -u

# JOINING THE TAILNET IS THE ONE MANUAL STEP, and it needs a human: a Tailscale
# auth key is a credential that grants a machine access to the tailnet, so it is
# not something this script may mint for itself.
#
#   On a machine already on the tailnet:  tailscale up --authkey <KEY>   (or mint
#   a key at https://login.tailscale.com/admin/settings/keys and export TS_AUTHKEY)
#   Then, on this VM:                     tailscale up --authkey "$TS_AUTHKEY" \
#                                           --hostname fleet-selfheal --accept-routes
#
# MEASURED 2026-10-27: a Maritime VM is NOT on the tailnet by default and cannot
# reach 100.87.28.18, and the hub's fleet API is deliberately tailnet-only (nginx
# binds :8788 to the tailnet address, not to 0.0.0.0). So the watcher is blind
# until the VM joins. If TS_AUTHKEY is present this script joins on first run,
# which makes the remaining step a single variable rather than a procedure.
if [ -n "${TS_AUTHKEY:-}" ] && ! command -v tailscale >/dev/null 2>&1; then
  say "installing tailscale (first run)"
  curl -fsSL https://tailscale.com/install.sh 2>/dev/null | sh >/dev/null 2>&1
fi
if [ -n "${TS_AUTHKEY:-}" ] && command -v tailscale >/dev/null 2>&1; then
  if ! tailscale status >/dev/null 2>&1; then
    tailscale up --authkey "$TS_AUTHKEY" --hostname fleet-selfheal --accept-routes >/dev/null 2>&1
  fi
fi

HUB="${FLEET_HUB:-http://100.87.28.18:8788}"
HUB_TAILNET="${FLEET_HUB_TAILNET:-100.87.28.18}"
STATE="${FLEET_SELFHEAL_STATE:-/tmp/fleet-selfheal.state}"
TIMEOUT=20
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)

say() { printf '%s %s\n' "$now" "$*"; }

# 1. Is the hub reachable at all?
health=$(curl -s -m "$TIMEOUT" "$HUB/health" 2>/dev/null)
if [ -z "$health" ]; then
  # Distinguish "the API is down" from "the host is down": a ping that answers
  # while the API does not is a service fault, and the two need different fixes.
  if ! command -v tailscale >/dev/null 2>&1; then
    say "PROBLEM this VM is not on the tailnet, so it cannot see the hub at all"
    say "        fix: tailscale up --authkey <KEY> --hostname fleet-selfheal"
    echo "no-tailnet $now" > "$STATE"
    exit 1
  fi
  if ping -c 1 -W 3 "$HUB_TAILNET" >/dev/null 2>&1; then
    say "PROBLEM hub host answers ping but the fleet API on :8788 does not"
    say "        fix: on the hub, systemctl restart fleetd"
    echo "api-down $now" > "$STATE"
  else
    say "PROBLEM hub is UNREACHABLE on the tailnet ($HUB_TAILNET)"
    say "        a local timer on the hub could not have reported this"
    echo "host-down $now" > "$STATE"
  fi
  exit 1
fi

# 2. The hub is up. Ask it for its own verdict.
# The hub pretty-prints its JSON, so the value is on its OWN line. A single
# `.*"ok":\(.*\)` across the whole body matches the LAST field on the line
# instead, which read `ok=` and made a healthy hub look broken.
flat=$(printf '%s' "$health" | tr -d '\n')
ok=$(printf '%s' "$flat" | sed -n 's/.*"ok"[[:space:]]*:[[:space:]]*\([a-z]*\).*/\1/p')
harnesses=$(printf '%s' "$flat" | sed -n 's/.*"harnesses"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')
errors=$(printf '%s' "$flat" | sed -n 's/.*"load_errors"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')

if [ "$ok" != "true" ]; then
  say "PROBLEM hub /health reports ok=$ok"
  exit 1
fi
if [ -n "$errors" ] && [ "$errors" != "0" ]; then
  say "PROBLEM hub reports $errors harness load error(s)"
fi

# 3. Record the observation in shared memory, so the fleet's own history shows
#    the watcher running. A watcher that leaves no trace is indistinguishable
#    from a watcher that is dead.
prev=$(cat "$STATE" 2>/dev/null || echo "")
if [ "${prev%% *}" != "ok" ]; then
  say "recovered: hub is healthy again (was: ${prev:-unknown})"
fi
echo "ok $now" > "$STATE"

# 4. Write a heartbeat through the hub's own memory API. Best-effort: a failure
#    here is not a fleet failure, and must not be reported as one.
key=$(grep -h '^TB_API_KEY=' /opt/fleet/.env /root/.fleet/env 2>/dev/null | head -1 | cut -d= -f2-)
if [ -n "$key" ]; then
  curl -s -m "$TIMEOUT" -X POST "$HUB/v1/memory" \
    -H "Content-Type: application/json" \
    -d "{\"scope\":\"selfheal\",\"key\":\"vm-heartbeat\",\"value\":\"$now harnesses=$harnesses ok=$ok\",\"author\":\"maritime-selfheal\"}" \
    >/dev/null 2>&1
fi

say "ok harnesses=$harnesses load_errors=${errors:-0}"
exit 0
