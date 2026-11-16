#!/usr/bin/env bash
# Runs a command under Rig's machine-wide heavy lock on the Rig host (one heavy build or gate at a time; while it waits it says
# its place in line, the holder and its timeout). Elsewhere, with no Rig checkout, the command runs as given.
# Usage: res/heavy.sh cargo build --release ... | res/heavy.sh flutter build linux ...
set -euo pipefail
rig=/srv/rig/rig
[ -d "$rig" ] || exec "$@"
[ -f "$rig/scripts/heavy_lock.py" ] || { echo "res/heavy.sh: $rig is here but $rig/scripts/heavy_lock.py is not; nothing ran" >&2; exit 2; }
exec python3 "$rig/scripts/heavy_lock.py" --entry "rigdesk: $*" -- "$@"
