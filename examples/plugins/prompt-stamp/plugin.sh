#!/bin/sh
# The event name arrives as $1; the JSON payload arrives on stdin. A plugin
# that does not need the payload can simply ignore it — but it must read stdin,
# or a large payload can block the engine.
event="$1"
cat >/dev/null
case "$event" in
  pre_call) echo '{"prompt_prefix":"[fleet] "}' ;;
  *)        echo '{}' ;;
esac
