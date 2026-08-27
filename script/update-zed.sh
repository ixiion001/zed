#!/usr/bin/env bash
#
# Updates this fork's patched Zed build from its GitHub releases, on macOS and
# Linux. The macOS/Linux counterpart of script/update-zed.ps1, and deliberately
# the same shape as it.
#
# These builds are dev-channel on purpose. A stable-channel binary would let
# Zed's own updater download official Zed over the top and silently discard the
# Claude Code IDE patch, so that updater is off and this takes its place.
#
# Reads /releases/latest, which GitHub defines to exclude prereleases. That
# exclusion is the entire promotion gate: the weekly rebase publishes a
# prerelease, and no machine moves until someone promotes it by hand. --pre opts
# in for testing.
#
# Usage:
#   ./update-zed.sh [--pre] [--dry-run] [--install-dir DIR] [--repo OWNER/NAME]
#
#   --pre          consider prereleases too; use this to try a build before
#                  promoting it
#   --dry-run      report what would happen and change nothing
#   --install-dir  override the install location
#   --repo         override the source repository
#
# Requires curl, python3 and tar. Desktop installs have all three; minimal
# server and container images often ship none of them.

set -euo pipefail

REPO=ixiion001/zed
PRE=0
DRY=0
INSTALL_DIR=

while [ $# -gt 0 ]; do
    case "$1" in
        --pre) PRE=1 ;;
        --dry-run) DRY=1 ;;
        --install-dir) INSTALL_DIR=${2:?--install-dir needs a path}; shift ;;
        --repo) REPO=${2:?--repo needs OWNER/NAME}; shift ;;
        -h|--help) awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; exit 0 ;;
        *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

# --- platform ----------------------------------------------------------------
#
# Every leg of cc-release.yml publishes as zed-claude-code-<os>-<arch>, so the
# names below are spelled out in full rather than matched by pattern: a rename
# upstream then fails loudly here instead of silently finding nothing to do.

# The asset names also fix an architecture. Nothing downstream would notice a
# mismatch -- the download succeeds, the checksum verifies, the bundle installs,
# and the only symptom is an exec-format error at launch -- so refuse here,
# where the message can say what went wrong.

case "$(uname -s)" in
    Darwin)
        # Apple silicon only; uname -m spells that arm64, the triple aarch64.
        if [ "$(uname -m)" != arm64 ]; then
            echo "unsupported architecture: $(uname -m)." >&2
            echo "Only Apple silicon (arm64) is published; build from source for Intel." >&2
            exit 1
        fi
        PLATFORM=macos
        ASSET=zed-claude-code-macos-aarch64.dmg
        INSTALL_DIR=${INSTALL_DIR:-/Applications}
        APP="$INSTALL_DIR/Zed Dev.app"
        CLI="$APP/Contents/MacOS/cli"
        ;;
    Linux)
        if [ "$(uname -m)" != x86_64 ]; then
            echo "unsupported architecture: $(uname -m)." >&2
            echo "Only x86_64 is published; build from source for aarch64." >&2
            exit 1
        fi
        PLATFORM=linux
        ASSET=zed-claude-code-linux-x86_64.tar.gz
        INSTALL_DIR=${INSTALL_DIR:-$HOME/.local}
        APP="$INSTALL_DIR/zed-dev.app"
        CLI="$APP/bin/zed"
        ;;
    *)
        echo "unsupported platform: $(uname -s). Windows uses update-zed.ps1." >&2
        exit 1
        ;;
esac

for tool in curl python3 tar; do
    command -v "$tool" >/dev/null || { echo "$tool is required but not installed" >&2; exit 1; }
done

# GNU coreutils on Linux, BSD shasum on macOS. Both read the same
# "<hash>  <filename>" format the release assets carry.
if command -v sha256sum >/dev/null; then
    SHA256SUM=(sha256sum)
elif command -v shasum >/dev/null; then
    SHA256SUM=(shasum -a 256)
else
    echo "need sha256sum or shasum to verify the download" >&2
    exit 1
fi

API="https://api.github.com/repos/$REPO"
CURL=(curl --silent --show-error --location
      --header 'Accept: application/vnd.github+json'
      --header 'User-Agent: update-zed')

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Reads one value out of a JSON file without depending on jq, which is not
# installed by default on macOS.
json() {  # <file> <python expression over `d`>
    python3 -c 'import json,sys
d = json.load(open(sys.argv[1]))
v = eval(sys.argv[2])
print("" if v is None else v)' "$1" "$2"
}

# --- what is installed -------------------------------------------------------
#
# Preferred, and stateless: the build stamps its commit into the version string,
# so asking the installed CLI stays correct even for a copy someone unpacked by
# hand. That works on Linux.
#
# It cannot work on macOS. There the CLI takes a different path entirely --
# `mac_os::Bundle::zed_version_string` in crates/cli/src/main.rs formats
# `CFBundleShortVersionString` out of Info.plist, which carries neither the
# channel nor the commit, so `--version` reports a bare `Zed 1.16.3`. Official
# Zed prints the same shape, which makes it look like a stable build rather than
# a missing field. Embedding the commit in the bundle instead would invalidate
# its ad-hoc signature, so this records what it installed and reads that back.
#
# Anything unreadable reports "nothing found", which installs rather than skips
# -- the same fallback update-zed.ps1 takes.

STATE_DIR=${XDG_STATE_HOME:-$HOME/.local/state}/zed-claude-code
STATE_FILE="$STATE_DIR/installed-commit"

installed=
if [ -x "$CLI" ]; then
    installed=$("$CLI" --version 2>/dev/null | grep -oE '[0-9a-f]{40}' | head -1 || true)
fi
# cc-release.yml stamps the commit into the macOS bundle's Info.plist, before
# cargo-bundle builds it and therefore inside the signature. That is stateless
# again, so it also covers a bundle someone dragged out of the DMG by hand --
# which is what the release notes tell people to do. Filtered through grep
# because PlistBuddy reports a missing key on stdout, not stderr, and the error
# text would otherwise be taken for a commit.
if [ -z "$installed" ] && [ -f "$APP/Contents/Info.plist" ]; then
    installed=$(/usr/libexec/PlistBuddy -c 'Print :ZedCommitSha' "$APP/Contents/Info.plist" \
        2>/dev/null | grep -oE '^[0-9a-f]{40}$' | head -1 || true)
fi
# Releases predating that stamp have no such key, so keep reading back what this
# script recorded when it installed.
if [ -z "$installed" ] && [ -e "$APP" ] && [ -r "$STATE_FILE" ]; then
    installed=$(grep -oE '^[0-9a-f]{40}$' "$STATE_FILE" 2>/dev/null | head -1 || true)
fi

short=${installed:0:10}
echo "install dir : $INSTALL_DIR"
echo "installed   : ${short:-nothing found}"

# Sweep up a bundle a previous run moved aside. Done before deciding whether an
# update is due, because otherwise a several-hundred-MB leftover would sit there
# until the *next* update rather than the next run.
if [ "$DRY" -eq 0 ]; then
    rm -rf "$APP".old-* 2>/dev/null || true
fi

# --- which release -----------------------------------------------------------

release="$TMP/release.json"
if [ "$PRE" -eq 1 ]; then
    code=$("${CURL[@]}" --write-out '%{http_code}' -o "$TMP/list.json" "$API/releases?per_page=10")
    if [ "$code" != 200 ]; then
        echo "GitHub returned HTTP $code listing releases for $REPO" >&2
        exit 1
    fi
    python3 -c 'import json,sys
data = json.load(open(sys.argv[1]))
# An error response is a JSON object, not the expected array; iterating it would
# raise TypeError and bury the real message in a traceback.
releases = [r for r in data if not r["draft"]] if isinstance(data, list) else []
json.dump(releases[0] if releases else {}, open(sys.argv[2], "w"))' "$TMP/list.json" "$release"
else
    # 404 is the normal state while only prereleases exist: nothing has been
    # promoted, so there is nothing to update to. Not an error.
    code=$("${CURL[@]}" --write-out '%{http_code}' -o "$release" "$API/releases/latest")
    if [ "$code" = 404 ]; then
        # 404 is ambiguous: the repository exists with nothing promoted yet, or
        # it does not exist at all -- a typo in --repo reads exactly the same.
        # Only the first is normal, so spend one extra request to tell them
        # apart. Reporting a misspelled repository as "up to date" is the same
        # quiet no-op as the rate limit above, and just as misleading.
        repo_code=$("${CURL[@]}" --write-out '%{http_code}' -o /dev/null "$API")
        if [ "$repo_code" != 200 ]; then
            echo "repository $REPO not found (HTTP $repo_code)" >&2
            exit 1
        fi
        echo '{}' > "$release"
    elif [ "$code" != 200 ]; then
        echo "GitHub returned HTTP $code for /releases/latest" >&2
        exit 1
    fi
fi

tag=$(json "$release" 'd.get("tag_name")')
if [ -z "$tag" ]; then
    echo 'latest      : none promoted yet'
    echo 'up to date. Re-run with --pre to try an unpromoted prerelease.'
    exit 0
fi

"${CURL[@]}" "$API/commits/$tag" -o "$TMP/commit.json"
commit=$(json "$TMP/commit.json" 'd.get("sha")')
# An empty sha means the API answered with something other than a commit --
# most often the unauthenticated rate limit, which is 60 requests an hour and
# this run spends two or three. Left unchecked it compares equal to an empty
# `installed` on a machine with nothing installed, and the script would report
# "up to date" and exit 0 having installed nothing at all.
if [ -z "$commit" ]; then
    echo "could not resolve $tag to a commit; GitHub returned no sha." >&2
    echo "(unauthenticated requests are capped at 60/hour -- try again later)" >&2
    exit 1
fi
prerelease=$(json "$release" 'd.get("prerelease")')
label="$tag"
[ "$prerelease" = "True" ] && label="$tag (prerelease)"
echo "latest      : $label  ${commit:0:10}"

if [ "$commit" = "$installed" ]; then
    echo 'up to date.'
    exit 0
fi

url=$(json "$release" "next((a['browser_download_url'] for a in d['assets'] if a['name'] == '$ASSET'), None)")
sum_url=$(json "$release" "next((a['browser_download_url'] for a in d['assets'] if a['name'] == '$ASSET.sha256'), None)")
size=$(json "$release" "next((a['size'] for a in d['assets'] if a['name'] == '$ASSET'), 0)")
if [ -z "$url" ] || [ -z "$sum_url" ]; then
    echo "release $tag has no $ASSET and matching .sha256" >&2
    echo "(this platform may not be built for that release yet)" >&2
    exit 1
fi

mb=$(( size / 1024 / 1024 ))
if [ "$DRY" -eq 1 ]; then
    echo "would update: ${short:-fresh install} -> ${commit:0:10}  (${mb} MB)"
    exit 0
fi

# --- download and verify -----------------------------------------------------

echo "downloading : ${mb} MB"
"${CURL[@]}" "$url" -o "$TMP/$ASSET"
"${CURL[@]}" "$sum_url" -o "$TMP/$ASSET.sha256"

# The checksum file names the asset without a directory, so verify from inside
# the directory holding it.
( cd "$TMP" && "${SHA256SUM[@]}" -c "$ASSET.sha256" >/dev/null ) || {
    echo "checksum mismatch on $ASSET. Refusing to install." >&2
    exit 1
}
echo 'checksum    : OK'

# --- install -----------------------------------------------------------------
#
# Move the old bundle aside rather than deleting or writing over it: on both
# platforms a running application can be renamed, but overwriting one that is
# open fails partway and leaves the install unlaunchable. The old copy is
# collected by the next run, once nothing is using it.

stamp=${short:-unknown}
staging="$TMP/staging"
mkdir -p "$staging"

case "$PLATFORM" in
    macos)
        mount=$(mktemp -d)
        # bundle-mac attaches a licence agreement to the image (dmg-license, at
        # bundle-mac:269), and hdiutil waits for that to be accepted before it
        # will mount. The herestring answers it. Not `yes |`: `yes` takes SIGPIPE
        # when hdiutil exits, and `set -o pipefail` would turn that into a
        # failure. PAGER=cat stops hdiutil paging the licence text and blocking.
        # -nobrowse keeps the volume out of Finder.
        env PAGER=cat hdiutil attach "$TMP/$ASSET" -mountpoint "$mount" -nobrowse -quiet <<< "Y"
        # Copy out before detaching: the DMG is read-only and goes away below.
        cp -R "$mount"/*.app "$staging"/
        hdiutil detach "$mount" -quiet
        rmdir "$mount" 2>/dev/null || true
        ;;
    linux)
        # bundle-linux packs a top-level zed-dev.app/ directory.
        tar -xzf "$TMP/$ASSET" -C "$staging"
        ;;
esac

new=$(find "$staging" -maxdepth 1 -name '*.app' -print -quit)
[ -n "$new" ] || { echo "no application bundle inside $ASSET" >&2; exit 1; }

mkdir -p "$INSTALL_DIR"
if [ -e "$APP" ]; then
    mv "$APP" "$APP.old-$stamp"
fi
mv "$new" "$APP"
# The old bundle is deliberately left behind. A running Zed still has its
# frameworks and resources mapped from that path, and deleting it here pulls
# them out from under a live process -- the exact failure the rename above
# exists to avoid. The sweep near the top of this script collects it on the next
# run, by which time nothing is using it.

if [ "$PLATFORM" = macos ]; then
    # Ad-hoc signed, so the download carries a quarantine flag and macOS reports
    # the app as damaged rather than merely unsigned. Clearing it is what the
    # release notes tell users to do by hand; doing it here means an update
    # never reintroduces the problem.
    xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
else
    # ~/.local/bin is on PATH by default on most distributions; the symlink is
    # what makes `zed` work from a shell.
    mkdir -p "$INSTALL_DIR/bin"
    ln -sfn "$APP/bin/zed" "$INSTALL_DIR/bin/zed"
fi

# Record what was installed, for the platforms whose artifact cannot say.
mkdir -p "$STATE_DIR"
printf '%s\n' "$commit" > "$STATE_FILE"

now=
[ -x "$CLI" ] && now=$("$CLI" --version 2>/dev/null | head -1 || true)
echo "installed   : ${now:-${commit:0:10}}"
echo "done. Restart Zed for the new build to take effect."
