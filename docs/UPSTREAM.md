# Taking a new upstream release

RigDesk follows RustDesk's release tags. It is based on v1.5.1 (`78450f4`), with `libs/hbb_common` pointing at
[deussalo/rigdesk-hbb_common](https://github.com/deussalo/rigdesk-hbb_common), which carries one commit of ours on top
of upstream's pin (no public rendezvous server, key or version-check URL).

Upstream is merged in, not rebased onto: `master` already holds merge commits, and a merge keeps every release we
tagged reachable. Do it on a branch whose name starts with `s` or on a PR, so CI builds it before `master` moves.

## 1. hbb_common first

```bash
cd libs/hbb_common
git remote add upstream https://github.com/rustdesk/hbb_common.git   # once
git fetch upstream
NEW_PIN=$(git -C ../.. ls-tree <tag> libs/hbb_common | awk '{print $3}')   # after step 2's fetch
git checkout -b up-<tag> origin/main && git merge $NEW_PIN
git push origin up-<tag>:main
```

Keep our commit's intent: no public host or key in `config.rs` (`RENDEZVOUS_SERVERS`, `RS_PUB_KEY`, the version-check
URL). A new public host upstream added belongs in the same commit.

## 2. Merge the tag

```bash
git fetch upstream --tags
git checkout -b s-up-<tag> master
git merge <tag>
```

Expected conflicts, and how they resolve:

- **Files we deleted** (modify/delete): Sciter UI (`src/ui*`), languages other than `en`, Android/iOS, Linux
  packaging, translated docs, every workflow except `windows.yml`. Keep them deleted:
  `git status --porcelain | awk '$1 ~ /^(DU|UD)$/ {print $2}' | xargs -r git rm -q`.
  A new language file upstream added is deleted too, and `src/lang.rs` keeps only `mod en`.
- **`.github/workflows/windows.yml`**: ours, derived from upstream's `flutter-build.yml`. Port toolchain bumps
  (`RUST_VERSION`, `FLUTTER_VERSION`, `VCPKG_COMMIT_ID`, the bridge versions) by hand.
- **Hooks in shared files**, each a few lines: `src/server.rs` (agent server, enrollment), `src/server/connection.rs`
  (viewer joins, leaves, `AgentRequest`, input takes the holder back), `src/client.rs` and `src/client/io_loop.rs`
  (`AgentState`), `src/flutter.rs` and `src/flutter_ffi.rs` (`agent_state`, `session_agent_request`),
  `flutter/lib/models/model.dart` and `flutter/lib/desktop/pages/remote_page.dart` (the overlay),
  `src/platform/windows.rs` (the tailnet-only firewall rule, PowerShell as the signed-in user).
- **Network (S1)**: `src/common.rs` (embedded `res/rigdesk.json`, no `custom.txt`), the rendezvous mediator kept off,
  the updater and API server gone. Re-check any new code path that dials out.
- **`libs/base/protos/message.proto`**: `agent_request = 40` and `agent_state = 41` in `Message`. If upstream took
  either number, move ours to the next free ones; viewer and PC must then come from the same release.

Then point the submodule at step 1's commit: `git -C libs/hbb_common checkout origin/main && git add libs/hbb_common`.

## 3. Check what upstream added

```bash
git diff <old-tag> <tag> -- src libs flutter/lib | grep -E '^\+.*https?://' | grep -v '^+\s*//'
git diff <old-tag> <tag> -- libs/base/src/config/keys.rs
```

- A new URL that the PC or viewer would contact by itself is removed or switched off. CI's binary grep catches
  RustDesk's hosts and keys, not a new third-party host.
- A new server-side permission option (`enable-*`) is added to `res/rigdesk.json` as `N` unless the viewer needs it.
- A new hidden-settings option for server, network, proxy or security is set to hide.

## 4. CI, then release

Push the branch. It is ready when every job is green: the bridge, the build, the binary grep, the agent smoke test
against Notepad, the installer smoke test, and the unit tests (`cargo test -p rigdesk-agent`,
`flutter/test/agent_share_test.dart`). Locally, `cargo test -p rigdesk-agent` and the plugin tests run without
Windows.

Merge into `master` and tag the next `vX.Y.Z`. The tag's run publishes the release: the zip, `install.ps1` pinned to
the tag and the zip's SHA-256, `SHA256SUMS`, and notes with the install line. A PC moves to the release when that
line is run on it again.
