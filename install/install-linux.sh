#!/bin/bash
# Enroll a Linux PC as a RigDesk desk: Tailscale (tagged), RigDesk as the rigdesk package and its boot service, and the
# PC's secrets. Run once, as root, from the line a RigDesk release gives:
#
#   sudo bash install-linux.sh --client acme --user deus < secrets
#
# The secrets come on stdin, one key=value a line, so no command line or shell history ever holds one:
#   tailscale_auth_key=tskey-auth-...   (left out: Tailscale must already be up)
#   password=...                         the permanent password, 8-64 characters
#   pin=...                              the admin settings PIN, 4-32 characters
#   agent_token=...                      the desktop agent API's token, 32-128 of A-Za-z0-9_-
#   lane_token=...                       the management lane's token, 32-128 of A-Za-z0-9_-, not the agent token
#
# It installs the package (checking its SHA-256), which replaces a stock rustdesk package and leaves a RustDesk Flatpak
# alone; hands the lane token and --agent-shell on|off (on by default: Rig's agents may run commands as root or as the
# signed-in user, every one logged) to the root service through /var/lib/rigdesk/enroll.json, and the password, PIN,
# agent token and --agent-join on|off to the desktop user's server through their own enroll.json, each 0600 and deleted
# once applied; lets the direct port 21118 in from the tailnet only; and publishes the agent API at / and the lane at
# /lane with `tailscale serve`. Rerunning it with new secrets rotates them. It never prints a secret.
set -euo pipefail

DEB_URL='https://github.com/deussalo/rigdesk/releases/download/__TAG__/rigdesk-linux-x64.deb'
DEB_SHA256='__SHA256__'
CLIENT='' USER_NAME='' AGENT_SHELL=on AGENT_JOIN=on DEB='' SKIP_TAILSCALE=0

say() { echo "==> $*"; }
die() { echo "install-linux: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --client) CLIENT=$2; shift 2 ;;
        --user) USER_NAME=$2; shift 2 ;;
        --agent-shell) AGENT_SHELL=$2; shift 2 ;;
        --agent-join) AGENT_JOIN=$2; shift 2 ;;
        # For CI and for testing a build: a local package instead of the release's, and no Tailscale.
        --deb) DEB=$2; shift 2 ;;
        --skip-tailscale) SKIP_TAILSCALE=1; shift ;;
        *) die "unknown argument $1" ;;
    esac
done

[ "$(id -u)" = 0 ] || die "run it as root (sudo bash install-linux.sh ...)"
[[ "$CLIENT" =~ ^[a-z0-9][a-z0-9-]{0,62}$ ]] || die "--client is a slug: a-z, 0-9 and -, up to 63"
[[ "$AGENT_SHELL" =~ ^(on|off)$ ]] || die "--agent-shell is on or off"
[[ "$AGENT_JOIN" =~ ^(on|off)$ ]] || die "--agent-join is on or off"
[ -d /run/systemd/system ] || die "this PC does not run systemd, which RigDesk's service and agent shell need"
USER_LINE=$(getent passwd "$USER_NAME") || die "--user names the PC's desktop user, and $USER_NAME is not one"
USER_UID=$(echo "$USER_LINE" | cut -d: -f3); USER_GID=$(echo "$USER_LINE" | cut -d: -f4); USER_HOME=$(echo "$USER_LINE" | cut -d: -f6)
[ "$USER_UID" -ge 1000 ] || die "$USER_NAME is a system account, not the desktop user"

declare -A SECRET=()
while IFS= read -r line || [ -n "$line" ]; do
    [ -z "$line" ] && continue
    key=${line%%=*}; value=${line#*=}
    case "$key" in tailscale_auth_key|password|pin|agent_token|lane_token) SECRET[$key]=$value ;; *) die "unknown secret $key" ;; esac
done
for key in password pin agent_token lane_token; do [ -n "${SECRET[$key]:-}" ] || die "no $key on stdin"; done
len=${#SECRET[password]}; [ "$len" -ge 8 ] && [ "$len" -le 64 ] || die "the password is 8-64 characters"
len=${#SECRET[pin]}; [ "$len" -ge 4 ] && [ "$len" -le 32 ] || die "the PIN is 4-32 characters"
for key in agent_token lane_token; do [[ "${SECRET[$key]}" =~ ^[A-Za-z0-9_-]{32,128}$ ]] || die "$key is 32-128 of A-Za-z0-9_-"; done
[ "${SECRET[agent_token]}" != "${SECRET[lane_token]}" ] || die "the lane token must differ from the agent token: the desktop user can read the agent token"
for key in password pin; do [[ "${SECRET[$key]}" != *[[:cntrl:]]* ]] || die "the $key holds a control character"; done

# A JSON string, for values checked above.
json() { local s=${1//\\/\\\\}; s=${s//\"/\\\"}; printf '"%s"' "$s"; }

if [ "$SKIP_TAILSCALE" = 0 ]; then
    if ! command -v tailscale > /dev/null; then
        say "installing Tailscale"
        curl -fsSL https://tailscale.com/install.sh | sh
    fi
    if [ -n "${SECRET[tailscale_auth_key]:-}" ]; then
        say "joining the tailnet as tag:pc,tag:client-$CLIENT"
        # From a 0600 file, never the command line, which every local user can read in /proc.
        KEY=$(umask 077; mktemp)
        trap 'rm -f "$KEY"' EXIT
        printf '%s' "${SECRET[tailscale_auth_key]}" > "$KEY"
        tailscale up --auth-key="file:$KEY" --advertise-tags="tag:pc,tag:client-$CLIENT" --hostname="$(hostname | tr 'A-Z' 'a-z')" > /dev/null
        rm -f "$KEY"
    fi
    tailscale status > /dev/null || die "Tailscale is not up: give tailscale_auth_key"
fi

PKG=$(mktemp --suffix=.deb)
trap 'rm -f "$PKG" "${KEY:-}"' EXIT
if [ -n "$DEB" ]; then
    cp "$DEB" "$PKG"
else
    [[ "$DEB_SHA256" =~ ^[0-9a-f]{64}$ ]] || die "no package SHA-256 pinned: use the line from a RigDesk release"
    say "downloading RigDesk"
    curl -fsSL -o "$PKG" "$DEB_URL"
    echo "$DEB_SHA256  $PKG" | sha256sum -c --quiet - || die "package SHA-256 mismatch"
fi
chmod 644 "$PKG"

say "handing the secrets to RigDesk"
install -d -m 700 /var/lib/rigdesk
printf '{"lane_token":%s,"agent_shell":%s}\n' "$(json "${SECRET[lane_token]}")" "$(json "$AGENT_SHELL")" | install -m 600 -o root -g root /dev/stdin /var/lib/rigdesk/enroll.json
USER_AGENT_DIR="$USER_HOME/.config/rigdesk/agent"
runuser -u "$USER_NAME" -- install -d -m 700 "$USER_HOME/.config/rigdesk" "$USER_AGENT_DIR"
# Written as the user, so a link they planted on the way cannot make root write elsewhere. The user reads the inherited stdin, not
# /dev/stdin, which they may not open; the mode is set outright and checked, since the file holds the PC's password and agent token.
runuser -u "$USER_NAME" -- bash -c 'umask 077; cat > "$1"; chmod 600 "$1"' _ "$USER_AGENT_DIR/enroll.json" <<JSON
{"password":$(json "${SECRET[password]}"),"pin":$(json "${SECRET[pin]}"),"agent_token":$(json "${SECRET[agent_token]}"),"agent_join":$(json "$AGENT_JOIN")}
JSON
[ "$(stat -c '%a %u' "$USER_AGENT_DIR/enroll.json")" = "600 $USER_UID" ] || die "$USER_AGENT_DIR/enroll.json is not $USER_NAME's alone (0600)"

say "installing the rigdesk package"
DEBIAN_FRONTEND=noninteractive apt-get install -y -q "$PKG" > /dev/null
systemctl restart rigdesk.service
# The desktop user's server applies its enroll.json when it starts; restart it if they are signed in.
pkill -u "$USER_UID" -f '^/usr/(bin|share/rigdesk)/rigdesk --server' || true

for _ in $(seq 100); do [ -S /run/rigdesk/lane.sock ] && [ ! -e /var/lib/rigdesk/enroll.json ] && break; sleep 0.2; done
[ ! -e /var/lib/rigdesk/enroll.json ] || die "RigDesk's service did not take its enrollment: journalctl -u rigdesk"
[ -S /run/rigdesk/lane.sock ] || die "RigDesk's service serves no lane: journalctl -u rigdesk"

if command -v nft > /dev/null; then
    say "letting the direct port 21118 in from the tailnet only"
    nft -f - <<'NFT'
table inet rigdesk
delete table inet rigdesk
table inet rigdesk {
    chain input {
        type filter hook input priority 0; policy accept;
        tcp dport 21118 iifname "lo" accept
        tcp dport 21118 iifname "tailscale0" accept
        tcp dport 21118 drop
    }
}
NFT
    # rigdesk.service applies it again at every start, so it outlives a reboot.
    install -d -m 755 /etc/rigdesk
    nft list table inet rigdesk | sed '1i table inet rigdesk\ndelete table inet rigdesk' > /etc/rigdesk/firewall.nft
else
    echo "install-linux: nft is missing, so 21118 is not limited to the tailnet; RigDesk takes viewers from the tailnet alone anyway" >&2
fi

if [ "$SKIP_TAILSCALE" = 0 ]; then
    say "publishing the agent API at / and the lane at /lane"
    tailscale serve --bg --https=443 http://127.0.0.1:21119 > /dev/null
    tailscale serve --bg --https=443 --set-path=/lane unix:/run/rigdesk/lane.sock > /dev/null
fi
unset SECRET
say "done: RigDesk runs as rigdesk.service; the agent shell is $AGENT_SHELL, sessions started from Rig are $AGENT_JOIN"
