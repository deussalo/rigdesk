#!/bin/bash
# The rigdesk package, end to end, on a host with systemd as PID 1, apt and passwordless sudo (a GitHub runner):
#   .github/smoke/deb.sh <rigdesk.deb> <stock rustdesk.deb>
# A PC running stock RustDesk moves to RigDesk through install-linux.sh: the stock package goes, rigdesk.service runs as
# root and serves the lane; commands run as root; stock RustDesk cannot come back beside it; a rerun with
# --agent-shell off closes the shell; remove stops everything and keeps the enrollment, purge forgets it.
set -euo pipefail
DEB=$(realpath "$1"); STOCK=$(realpath "$2")
HERE=$(cd "$(dirname "$0")" && pwd)
SOCK=/run/rigdesk/lane.sock
token() { head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 40; }
LANE_TOKEN=$(token); AGENT_TOKEN=$(token)
fail() { echo "FAIL: $*" >&2; echo "--- journalctl -u rigdesk" >&2; sudo journalctl -u rigdesk --no-pager -n 80 >&2 || true; exit 1; }
ok() { echo "ok: $*"; }
lane() { sudo curl -sS --max-time 20 --unix-socket "$SOCK" -H "Authorization: Bearer $LANE_TOKEN" "$@"; }
shell() { lane -H 'content-type: application/json' -d "$1" http://lane/shell; }
AGENT='"agent":{"id":"agent_smoke","title":"Smoke","graph":"ci"}'
secrets() { printf 'password=smoke-password-1\npin=4321\nagent_token=%s\nlane_token=%s\n' "$AGENT_TOKEN" "$LANE_TOKEN"; }
enroll() { secrets | sudo bash "$HERE/../../install/install-linux.sh" --client ci --user debtest --deb "$DEB" --skip-tailscale "$@"; }
installed() { [ "$(dpkg-query -W -f='${db:Status-Status}' "$1" 2>/dev/null)" = installed ]; }

id debtest > /dev/null 2>&1 || sudo useradd -m -s /bin/bash debtest
dpkg-deb -f "$DEB" Conflicts | grep -qx rustdesk || fail "the package does not conflict with rustdesk"

# The PC as it was: stock RustDesk, its service running.
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q "$STOCK" > /dev/null
installed rustdesk || fail "stock rustdesk did not install"
for _ in $(seq 50); do systemctl is-active --quiet rustdesk && break; sleep 0.2; done
systemctl is-active --quiet rustdesk || fail "stock rustdesk.service does not run"
ok "stock RustDesk installed and running"

# The move: install-linux.sh replaces it.
enroll
installed rustdesk && fail "stock rustdesk is still installed"
installed rigdesk || fail "rigdesk is not installed"
systemctl is-active --quiet rustdesk && fail "stock rustdesk.service still runs"
systemctl is-active --quiet rigdesk || fail "rigdesk.service does not run"
systemctl is-enabled --quiet rigdesk || fail "rigdesk.service does not start at boot"
[ -x /usr/bin/rustdesk ] && fail "/usr/bin/rustdesk is left behind"
pgrep -fa '^/usr/(bin|share/rigdesk)/rigdesk --service' > /dev/null || fail "no rigdesk --service process"
sudo test -e /var/lib/rigdesk/enroll.json && fail "the root enroll.json was not consumed"
[ "$(sudo stat -c '%a %U' /var/lib/rigdesk/agent)" = "700 root" ] || fail "/var/lib/rigdesk/agent is $(sudo stat -c '%a %U' /var/lib/rigdesk/agent)"
[ "$(sudo stat -c '%a %U' /var/lib/rigdesk/agent/lane.token)" = "600 root" ] || fail "lane.token is $(sudo stat -c '%a %U' /var/lib/rigdesk/agent/lane.token)"
sudo test -e /home/debtest/.config/rigdesk/agent/enroll.json || fail "the desktop user's enroll.json is missing"
got=$(sudo stat -c '%a %U' /home/debtest/.config/rigdesk/agent/enroll.json)
[ "$got" = "600 debtest" ] || fail "the user's enroll.json is not debtest's alone: $got"
ok "stock RustDesk replaced: rigdesk.service runs as root, enabled, enrollment applied"

# The lane, as root.
lane http://lane/health | jq -e '.os == "linux" and .lane == true and .shell.open == true' > /dev/null || fail "health: $(lane http://lane/health)"
out=$(shell "{$AGENT,\"command\":\"id -u; systemctl is-active rigdesk\",\"as\":\"system\"}")
echo "$out" | jq -e '.ok == true and (.text | test("\"exit_code\":0")) and (.text | test("0\\\\nactive"))' > /dev/null || fail "as root: $out"
if curl -sS --unix-socket "$SOCK" http://lane/health 2> /dev/null; then fail "an unprivileged user reached the lane"; fi
code=$(sudo curl -s -o /dev/null -w '%{http_code}' --unix-socket "$SOCK" -H "Authorization: Bearer $AGENT_TOKEN" http://lane/health)
[ "$code" = 401 ] || fail "the agent token reached the lane: HTTP $code"
for _ in $(seq 50); do grep -qs 'id -u' /var/log/rigdesk/managed-by-rig.txt && break; sleep 0.2; done
[ -r /var/log/rigdesk/managed-by-rig.txt ] && grep -q 'id -u' /var/log/rigdesk/managed-by-rig.txt || fail "the tray's list does not show the command"
ok "the lane runs commands as root (uid 0); root alone reaches it; the agent token does not; the tray's list shows it"

# Stock RustDesk cannot come back beside it.
if sudo dpkg -i "$STOCK" > /dev/null 2>&1; then fail "dpkg installed stock rustdesk beside rigdesk"; fi
installed rigdesk || fail "installing stock rustdesk removed rigdesk"
systemctl is-active --quiet rigdesk || fail "rigdesk.service stopped when stock rustdesk was refused"
ok "stock rustdesk is refused while rigdesk is installed"

# A rerun turns the switch off: the lane stays, the shell is closed.
enroll --agent-shell off
lane http://lane/health | jq -e '.shell.open == false' > /dev/null || fail "health after --agent-shell off: $(lane http://lane/health)"
out=$(shell "{$AGENT,\"command\":\"id -u\",\"as\":\"system\"}")
echo "$out" | jq -e '.code == "shell_off"' > /dev/null || fail "switch off: $out"
[ -e /var/log/rigdesk/managed-by-rig.txt ] && fail "the tray's list outlived the switch"
ok "--agent-shell off: shell_off, no tray list"

# Remove stops everything and keeps the enrollment; purge forgets it.
sudo DEBIAN_FRONTEND=noninteractive apt-get remove -y -q rigdesk > /dev/null
systemctl is-active --quiet rigdesk && fail "rigdesk.service runs after remove"
pgrep -f '^/usr/(bin|share/rigdesk)/rigdesk' > /dev/null && fail "a rigdesk process outlived remove: $(pgrep -fa '^/usr/(bin|share/rigdesk)/rigdesk')"
[ -e /usr/share/rigdesk/rigdesk ] && fail "the binary outlived remove"
sudo test -S "$SOCK" && fail "the lane socket outlived remove"
sudo test -e /var/lib/rigdesk/agent/lane.token || fail "remove dropped the enrollment"
sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y -q rigdesk > /dev/null
for path in /var/lib/rigdesk /var/log/rigdesk /run/rigdesk /root/.config/rigdesk; do sudo test -e "$path" && fail "$path outlived purge"; done
installed rigdesk && fail "rigdesk is still installed after purge"
ok "remove stops the service and keeps the enrollment; purge forgets it"
echo "deb smoke: all passed"
