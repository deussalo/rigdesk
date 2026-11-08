#!/usr/bin/env bash
# Package a RigDesk release: the Windows build as a zip, install.ps1 pinned to this tag and the zip's SHA-256, the
# checksums of both, and release notes carrying the install line (which checks install.ps1's own SHA-256).
# Usage: package.sh <tag>   (run from the repo root, with the build in ./rigdesk)
set -euo pipefail
tag="$1"
repo="${GITHUB_REPOSITORY:-deussalo/rigdesk}"
rm -rf out && mkdir out
(cd rigdesk && zip -qr -X ../out/rigdesk-windows-x64.zip .)
zip_sha=$(sha256sum out/rigdesk-windows-x64.zip | cut -d' ' -f1)
sed -e "s|__TAG__|$tag|g" -e "s|__SHA256__|$zip_sha|g" install/install.ps1 > out/install.ps1
grep -q "$zip_sha" out/install.ps1
! grep -q "__TAG__\|__SHA256__" out/install.ps1
script_sha=$(sha256sum out/install.ps1 | cut -d' ' -f1)
(cd out && sha256sum rigdesk-windows-x64.zip install.ps1 > SHA256SUMS)
script_sha_upper=$(echo "$script_sha" | tr a-f A-F)
cat > out/NOTES.md <<NOTES
RigDesk $tag for Windows x64. Source: https://github.com/$repo/tree/$tag (AGPL-3.0).

Enroll a PC: in an **administrator** PowerShell, run this one line with the PC's values (see the README):

\`\`\`powershell
[Net.ServicePointManager]::SecurityProtocol='Tls12'; \$f="\$env:TEMP\\rigdesk-install.ps1"; iwr -UseBasicParsing https://github.com/$repo/releases/download/$tag/install.ps1 -OutFile \$f; if ((Get-FileHash \$f -Algorithm SHA256).Hash -ne '$script_sha_upper') { throw 'install.ps1 checksum mismatch' }; & ([scriptblock]::Create((Get-Content -Raw \$f))) -Client <slug> -TailscaleAuthKey <tskey-auth-...> -Password <password> -Pin <pin> -AgentToken <token>; Remove-Item \$f
\`\`\`

The line checks install.ps1 against its SHA-256, and install.ps1 checks the package against its own.

| File | SHA-256 |
|---|---|
| rigdesk-windows-x64.zip | \`$zip_sha\` |
| install.ps1 | \`$script_sha\` |
NOTES
cat out/SHA256SUMS
