#!/usr/bin/env bash
# Agent API smoke test on Linux: the built server as the user, on an Xvfb display. Mints a grant through the debug-only
# route, claims it, then checks screenshot, input, clipboard, shell (exit status, time-out, a background job left
# running), the human holding input, and Block. Run under xvfb-run.
set -euo pipefail
exe=${1:-./rigdesk/rustdesk}
token="smoke-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
dir="$HOME/.config/rigdesk/agent"
mkdir -p "$dir" && chmod 700 "$dir"
printf '%s' "$token" > "$dir/api.token"
export RIGDESK_AGENT_DEBUG_MINT=1
base=http://127.0.0.1:21119
out="${RUNNER_TEMP:-/tmp}/agent-smoke"
mkdir -p "$out"
id=0
session=

fail() { echo "::error::agent smoke: $*"; exit 1; }
post() { curl -sS --fail-with-body -H "Authorization: Bearer $token" -H 'Content-Type: application/json' ${session:+-H "X-Desk-Session: $session"} -d "$2" "$base$1"; }
tool() {
  id=$((id + 1))
  local body reply
  body=$(jq -cn --arg name "$1" --argjson args "${2:-{\}}" --argjson id $id '{jsonrpc:"2.0",id:$id,method:"tools/call",params:{name:$name,arguments:$args}}')
  reply=$(post /mcp "$body")
  jq -e '.result' <<<"$reply" >/dev/null || fail "$1 gave no result: $reply"
  jq -c '.result' <<<"$reply"
}
text() { jq -r '[.content[] | select(.type == "text") | .text] | join("\n")' <<<"$1"; }
error() { jq -e '.isError == true' <<<"$1" >/dev/null; }

"$exe" --server > "$out/server.log" 2>&1 &
server=$!
trap 'kill $server 2>/dev/null || true; pkill -f "rustdesk --server" 2>/dev/null || true' EXIT

for _ in $(seq 60); do
  health=$(curl -sS -H "Authorization: Bearer $token" "$base/health" 2>/dev/null) && break || sleep 1
done
[ -n "${health:-}" ] || { ls -la "$HOME/.config" "$dir" || true; tail -50 "$out/server.log" || true; fail "the agent API did not answer on $base within 60 s"; }
echo "health: $health"

minted=$(post /debug/mint '{}')
claim=$(post /claim "$(jq -c '{grant, secret, agent: {id: "smoke", title: "Smoke", graph: "ci"}}' <<<"$minted")")
session=$(jq -r .session <<<"$claim")
[ "$(jq -r .standing.holder <<<"$claim")" = agent ] || fail "after claim the holder is not agent: $claim"

shot=$(tool screenshot)
png=$(jq -r '[.content[] | select(.type == "image") | .data][0] // ""' <<<"$shot")
{ ! error "$shot" && [ ${#png} -gt 100 ]; } || fail "screenshot is empty: $(text "$shot")"
base64 -d <<<"$png" > "$out/screen.png"
echo "screenshot: ${#png} base64 chars, $(text "$shot")"

for call in 'move {"x":100,"y":100}' 'click {"x":100,"y":100}' 'type {"text":"rigdesk"}' 'key {"key":"enter"}' 'shortcut {"keys":"ctrl+a"}' 'scroll {"dy":-1}'; do
  done_=$(tool ${call%% *} "${call#* }")
  ! error "$done_" || fail "${call%% *} refused: $(text "$done_")"
done
echo "input: move, click, type, key, shortcut and scroll answered"

words="rigdesk smoke $RANDOM"
set_=$(tool clipboard "$(jq -cn --arg t "$words" '{text:$t}')")
got=$(text "$(tool clipboard)")
[ "$got" = "$words" ] || fail "clipboard holds '$got', not '$words' ($(text "$set_"))"
echo "clipboard: $got"

ran=$(tool shell '{"command":"id -un; echo to-stderr >&2; exit 3"}')
answer=$(text "$ran")
{ ! error "$ran" && [ "$(jq -r .exit_code <<<"$answer")" = 3 ] && jq -r .output <<<"$answer" | grep -qx "$(id -un)" && jq -r .output <<<"$answer" | grep -qx to-stderr; } \
  || fail "shell did not answer id, stderr and exit 3: $answer"
echo "shell: $(jq -c . <<<"$answer")"

slow=$(tool shell '{"command":"echo begun; sleep 30","timeout_s":2}')
{ error "$slow" && text "$slow" | grep -q "timed out" && text "$slow" | grep -q begun; } || fail "a slow command was not ended as timed out: $(text "$slow")"
! pgrep -u "$(id -u)" -f "sleep 30" >/dev/null || fail "the timed-out command's sleep is still running"
echo "time-out: $(text "$slow" | head -1)"

kept=$(tool shell '{"command":"nohup sleep 120 >/dev/null 2>&1 & echo started"}')
answer=$(text "$kept")
[ "$(jq -r .exit_code <<<"$answer")" = 0 ] || fail "starting a background job failed: $answer"
sleep 1
pgrep -u "$(id -u)" -f "sleep 120" >/dev/null || fail "a background job the command started did not keep running"
pkill -u "$(id -u)" -f "sleep 120" || true
echo "background: a job the command started keeps running"

post /debug/viewer/human '{}' >/dev/null
held=$(tool type '{"text":"x"}')
{ error "$held" && text "$held" | grep -q "is using the PC"; } || fail "an action ran while the human held input: $(text "$held")"
read_=$(tool screenshot)
! error "$read_" || fail "a read was refused while the human held input: $(text "$read_")"
post /debug/viewer/hand '{}' >/dev/null
back=$(tool wait '{"ms":10}')
! error "$back" || fail "handing input back did not take effect: $(text "$back")"

post /debug/viewer/block '{}' >/dev/null
blocked=$(tool screenshot)
{ error "$blocked" && text "$blocked" | grep -q blocked; } || fail "a read ran while blocked: $(text "$blocked")"
echo "blocked: $(text "$blocked")"
echo "agent smoke passed"
