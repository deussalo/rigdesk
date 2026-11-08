"""Download one file for CI: `fetch_asset.py <url> <dst>`.

github.com/<owner>/<repo>/releases/download/<tag>/<name> URLs are fetched through the REST API's asset endpoint,
which keeps working while the release download pages answer 504. Other URLs are fetched as given. vcpkg calls this
through X_VCPKG_ASSET_SOURCES (x-script) and checks the SHA-512 of what lands in <dst> itself.
"""
import json
import os
import re
import sys
import time
import urllib.request

RELEASE = re.compile(r"^https://github\.com/([^/]+)/([^/]+)/releases/download/([^/]+)/([^/?#]+)$")


def request(url, accept=None):
    headers = {"User-Agent": "rigdesk-ci"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token and url.startswith("https://api.github.com/"):
        headers["Authorization"] = f"Bearer {token}"
    if accept:
        headers["Accept"] = accept
    return urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=120)


def source(url):
    m = RELEASE.match(url)
    if not m:
        return url, None
    owner, repo, tag, name = (urllib.request.unquote(g) for g in m.groups())
    with request(f"https://api.github.com/repos/{owner}/{repo}/releases/tags/{urllib.request.quote(tag)}") as r:
        assets = json.load(r)["assets"]
    asset = next((a for a in assets if a["name"] == name), None)
    if asset is None:
        raise RuntimeError(f"{owner}/{repo} release {tag} has no asset {name}")
    return asset["url"], "application/octet-stream"


def main():
    url, dst = sys.argv[1], sys.argv[2]
    for attempt in range(1, 7):
        try:
            src, accept = source(url)
            tmp = dst + ".part"
            with request(src, accept) as r, open(tmp, "wb") as f:
                while chunk := r.read(1 << 20):
                    f.write(chunk)
            os.replace(tmp, dst)
            print(f"fetched {url}", file=sys.stderr)
            return 0
        except Exception as e:  # noqa: BLE001 - every failure is retried, then reported
            print(f"fetch {url} failed (attempt {attempt}): {e}", file=sys.stderr)
            time.sleep(10 * attempt)
    return 1


if __name__ == "__main__":
    sys.exit(main())
