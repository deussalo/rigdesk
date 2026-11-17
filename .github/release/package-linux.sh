#!/usr/bin/env bash
# Package RigDesk's Linux release: the rigdesk package, install-linux.sh pinned to this tag and the package's SHA-256,
# their checksums, and the notes carrying the install line (which checks install-linux.sh's own SHA-256).
# Usage: package-linux.sh <tag>   (run from the repo root, with the package at ./rigdesk-linux-x64.deb)
set -euo pipefail
tag="$1"
repo="${GITHUB_REPOSITORY:-deussalo/rigdesk}"
rm -rf out-linux && mkdir out-linux
cp rigdesk-linux-x64.deb out-linux/
deb_sha=$(sha256sum out-linux/rigdesk-linux-x64.deb | cut -d' ' -f1)
sed -e "s|__TAG__|$tag|g" -e "s|__SHA256__|$deb_sha|g" install/install-linux.sh > out-linux/install-linux.sh
grep -q "$deb_sha" out-linux/install-linux.sh
! grep -q "__TAG__\|__SHA256__" out-linux/install-linux.sh
script_sha=$(sha256sum out-linux/install-linux.sh | cut -d' ' -f1)
(cd out-linux && sha256sum rigdesk-linux-x64.deb install-linux.sh > SHA256SUMS-linux)
cat > out-linux/NOTES.md <<NOTES

## Linux x64 (Debian, Ubuntu)

Enroll a PC: put its secrets in a file only you can read (chmod 600; see the README), then run, as a user with sudo:

\`\`\`bash
f=\$(mktemp) && curl -fsSL -o "\$f" https://github.com/$repo/releases/download/$tag/install-linux.sh && echo "$script_sha  \$f" | sha256sum -c --quiet - && sudo bash "\$f" --client <slug> --user <desktop user> < secrets; rm -f "\$f"
\`\`\`

The line checks install-linux.sh against its SHA-256, and install-linux.sh checks the package against its own. The
rigdesk package replaces a stock rustdesk package and leaves a RustDesk Flatpak alone.

| File | SHA-256 |
|---|---|
| rigdesk-linux-x64.deb | \`$deb_sha\` |
| install-linux.sh | \`$script_sha\` |
NOTES
cat out-linux/SHA256SUMS-linux
