#!/bin/bash
# A Linux PC's management lane, end to end, on a host with systemd as PID 1 and passwordless sudo (a GitHub runner):
#   .github/smoke/lane.sh <lane_host binary>
# It runs the lane as root on a root-only socket and proves: commands as root and as a user, a job that outlives the
# lane's restart, the job cap, a time-out that ends the whole unit, the switch and the pause, and that nobody but root
# reaches the lane or reads its journal, and that it takes this PC's health samples with the shipped script.
set -euo pipefail
BIN=$(realpath "$1")
HEALTH=$(realpath "$(dirname "$0")/../../src/platform/linux/agent_health.sh")
DIR=/var/lib/rigdesk-smoke
SOCK=/run/rigdesk-smoke/lane.sock
TOKEN=$(head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 40)
LOG=${RUNNER_TEMP:-/tmp}/lane-smoke.log
# The unprivileged runner's own: it cannot write under /var/lib.
ERR=${RUNNER_TEMP:-/tmp}/lane-smoke.err
fail() { echo "FAIL: $*" >&2; echo "--- lane log" >&2; sudo cat "$LOG" >&2 2>/dev/null || true; exit 1; }
ok() { echo "ok: $*"; }

sudo rm -rf "$DIR" /run/rigdesk-smoke
sudo install -d -m 700 "$DIR" /run/rigdesk-smoke
echo -n "$TOKEN" | sudo tee "$DIR/token" > /dev/null
sudo chmod 600 "$DIR/token"
echo on | sudo tee "$DIR/agent-shell" > /dev/null
sudo chmod 644 "$DIR/agent-shell"

# A user with a systemd manager of its own and no seat, as whom commands "as the user" run.
id lanetest > /dev/null 2>&1 || sudo useradd -m -s /bin/bash lanetest
sudo loginctl enable-linger lanetest
LUID=$(id -u lanetest)
sudo systemctl start "user@$LUID.service"
# /run/user/UID is 0700, the user's own: only root sees into it.
for _ in $(seq 150); do sudo test -S "/run/user/$LUID/bus" && break; sleep 0.2; done
sudo test -S "/run/user/$LUID/bus" || fail "lanetest's user manager has no bus (is dbus-user-session installed?): $(systemctl status "user@$LUID.service" --no-pager 2>&1 | tail -5)"

# With `health`, the lane takes a health sample every 5 s; only the sample check wants that, as each runs a unit of its own.
start_lane() {
  local health=""; [ "${1:-}" = health ] && health="LANE_HOST_HEALTH='$HEALTH'"
  sudo bash -c "LANE_HOST_USER=lanetest $health nohup '$BIN' '$SOCK' '$DIR/token' '$DIR' >> '$LOG' 2>&1 & echo \$! > '$DIR/pid'"
  for _ in $(seq 100); do sudo grep -q listening "$LOG" 2>/dev/null && sudo test -S "$SOCK" && return; sleep 0.1; done
  fail "the lane did not start"
}
stop_lane() { sudo kill "$(sudo cat "$DIR/pid")"; sudo truncate -s 0 "$LOG"; }
lane() { sudo curl -sS --unix-socket "$SOCK" -H "Authorization: Bearer $TOKEN" "$@"; }
shell() { lane -H 'content-type: application/json' -d "$1" http://lane/shell; }
AGENT='"agent":{"id":"agent_smoke","title":"Smoke","graph":"ci"}'

start_lane health
lane http://lane/health | jq -e '.os == "linux" and .lane == true and .shell.open == true' > /dev/null || fail "health: $(lane http://lane/health)"

# The first sample comes 5 s after the start; the script runs as root in a unit of its own.
for _ in $(seq 60); do [ "$(lane 'http://lane/samples?after=0' | jq '.samples | length')" -gt 0 ] && break; sleep 0.5; done
sample=$(lane 'http://lane/samples?after=0' | jq -c '.samples[0]')
echo "$sample" | jq -e --arg k "$(uname -r)" '.taken == "ok" and .figures.schema == 1 and .figures.os.build == $k and .figures.ram.total_mb > 0
  and (.figures.volumes | map(select(.name == "/")) | length) == 1 and (.figures.updates.pending | type) == "number"' > /dev/null || fail "health sample: $sample"
ok "a health sample: $(echo "$sample" | jq -c '{took_ms, errors: .figures.errors, cpu_ms: .figures.cpu_ms}')"
stop_lane
for _ in $(seq 50); do [ -z "$(systemctl list-units --no-legend 'rigdesk-run-*')" ] && break; sleep 0.2; done
start_lane
ok "health says linux, lane, open"

# a. As root, and as the user.
out=$(shell "{$AGENT,\"command\":\"id -u; echo \$HOME; echo \$DISPLAY.\",\"as\":\"system\"}")
echo "$out" | jq -e '.ok == true and (.text | fromjson | .exit_code == 0 and (.output | rtrimstr("\n")) == "0\n/root\n.")' > /dev/null || fail "as root: $out"
ok "as root: uid 0, home /root, no DISPLAY"
# systemd expands ${NAME} and turns $$ into $ in a unit's command line: the command must reach the shell as written.
out=$(shell "{$AGENT,\"command\":\"X=ab; echo \\\"\${X}c \$(( \$\$ > 1 )) %h\\\"\",\"as\":\"system\"}")
echo "$out" | jq -e '.ok == true and (.text | fromjson | .output | rtrimstr("\n")) == "abc 1 %h"' > /dev/null || fail "a command with \${X} and \$\$ was changed on its way: $out"
out=$(shell "{$AGENT,\"command\":\"X=ab; echo \\\"\${X}c\\\"\",\"as\":\"user\"}")
echo "$out" | jq -e '.ok == true and (.text | fromjson | .output | rtrimstr("\n")) == "abc"' > /dev/null || fail "a user's command with \${X} was changed on its way: $out"
ok "commands reach the shell as written: \${X}, \$\$ and % untouched"
out=$(shell "{$AGENT,\"command\":\"id -un; pwd\",\"as\":\"user\"}")
echo "$out" | jq -e '.ok == true and (.text | test("lanetest\\\\n/home/lanetest"))' > /dev/null || fail "as the user: $out"
ok "as the user: lanetest in its home"
out=$(shell "{$AGENT,\"command\":\"sleep 1\",\"as\":\"user\",\"background\":true}")
echo "$out" | jq -e '.code == "bad_request"' > /dev/null || fail "a user job was not refused: $out"
ok "a background job as the user is refused (D4)"

# c. Nobody but root reaches the lane; a wrong token is refused.
if curl -sS --unix-socket "$SOCK" http://lane/health 2> "$ERR"; then fail "an unprivileged user reached the lane"; fi
grep -qi "permission denied\|couldn't connect" "$ERR" || fail "unexpected error for an unprivileged user: $(cat "$ERR")"
sudo -u lanetest curl -sS --unix-socket "$SOCK" http://lane/health 2> /dev/null && fail "lanetest reached the lane"
code=$(sudo curl -s -o /dev/null -w '%{http_code}' --unix-socket "$SOCK" -H "Authorization: Bearer wrong" http://lane/health)
[ "$code" = 401 ] || fail "a wrong token got HTTP $code"
lane -o /dev/null -w '%{http_code}' http://lane/claim -d '{}' | grep -q 404 || fail "the lane serves a desktop route"
if cat "$DIR/actions.jsonl" > /dev/null 2>&1; then fail "an unprivileged user read the journal"; fi
ok "only root reaches the lane; wrong token 401; no desktop routes; journal unreadable"

# The switch: off, or not root's, closes it.
echo off | sudo tee "$DIR/agent-shell" > /dev/null
out=$(shell "{$AGENT,\"command\":\"true\"}")
echo "$out" | jq -e '.code == "shell_off"' > /dev/null || fail "switch off: $out"
echo on | sudo tee "$DIR/agent-shell" > /dev/null
sudo chown lanetest "$DIR/agent-shell"
lane http://lane/health | jq -e '.shell.open == false and (.shell.words | test("not root"))' > /dev/null || fail "a switch lanetest owns was read: $(lane http://lane/health)"
sudo chown root "$DIR/agent-shell"
sudo grep -q '"outcome":"refused: shell_off"' "$DIR/actions.jsonl" || fail "the refusal is not journaled"
ok "switch off or not root's: shell_off, journaled"

# A time-out ends the whole unit, what it started in the background included.
out=$(shell "{$AGENT,\"command\":\"sleep 300 & sleep 300\",\"timeout_s\":2}")
echo "$out" | jq -e '.text | test("timed out|timed_out")' > /dev/null || fail "time-out: $out"
sleep 1
[ -z "$(systemctl list-units --no-legend 'rigdesk-run-*')" ] || fail "a unit outlived its time-out: $(systemctl list-units --no-legend 'rigdesk-run-*')"
pgrep -x sleep -u root > /dev/null && fail "a sleep outlived its time-out"
ok "time-out ends the unit and its background"

# b. Jobs: one outlives the lane's restart; the ninth is refused.
job=$(shell "{$AGENT,\"command\":\"echo started; sleep 600\",\"background\":true}" | jq -r '.text | fromjson | .job')
[[ "$job" == j-* ]] || fail "no job id"
stop_lane; start_lane
systemctl is-active --quiet "rigdesk-job-$job.service" || fail "the job did not outlive the lane's restart"
read=$(shell "{$AGENT,\"job\":\"$job\"}")
echo "$read" | jq -e '(.text | fromjson | .state) == "running" and (.text | test("started"))' > /dev/null || fail "job read after restart: $read"
ok "a job outlives the lane's restart and reads back"
for _ in $(seq 7); do shell "{$AGENT,\"command\":\"sleep 600\",\"background\":true}" | jq -e '.ok == true' > /dev/null || fail "jobs 2-8"; done
out=$(shell "{$AGENT,\"command\":\"sleep 600\",\"background\":true}")
echo "$out" | jq -e '.code == "jobs_full"' > /dev/null || fail "the ninth job: $out"
ok "the ninth job is refused"

# The pause ends running jobs and refuses new commands; resuming lets them run.
sudo touch "$DIR/paused"
out=$(shell "{$AGENT,\"command\":\"true\"}")
echo "$out" | jq -e '.code == "shell_paused"' > /dev/null || fail "paused: $out"
for _ in $(seq 100); do [ -z "$(systemctl list-units --no-legend --state=active 'rigdesk-job-*')" ] && break; sleep 0.1; done
[ -z "$(systemctl list-units --no-legend --state=active 'rigdesk-job-*')" ] || fail "jobs outlived the pause by 10 s"
sudo rm "$DIR/paused"
sleep 6
read=$(shell "{$AGENT,\"job\":\"$job\"}")
echo "$read" | jq -e '.text | test("paused the agent shell")' > /dev/null || fail "the paused job's end: $read"
shell "{$AGENT,\"command\":\"true\"}" | jq -e '.ok == true' > /dev/null || fail "after resume"
ok "pause ends jobs within 10 s and refuses; resume runs again"

stop_lane
echo "lane smoke: all passed"
