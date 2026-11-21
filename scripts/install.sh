#!/bin/sh
# scripts/install.sh — portable POSIX installer for pi-rs release binaries.
#
# Design rules (see docs/RELEASE.md):
#   * Downloads a prebuilt binary from the GitHub Releases of
#     hakergeniusz/pi-rs. Never pipes downloaded data into a shell.
#   * Verifies a SHA-256 checksum before touching the install prefix.
#   * Writes ONLY inside the chosen prefix (plus a temp dir under TMPDIR
#     that is removed on exit). No system-wide writes by default;
#     `--sudo` is strictly opt-in.
#   * Asset names are deterministic, so no JSON parsing is needed for
#     public releases. A token (GITHUB_TOKEN) enables the GitHub API
#     fallback needed by the currently-private repository.
#
# Typical use:
#   curl -fsSLO https://raw.githubusercontent.com/hakergeniusz/pi-rs/main/scripts/install.sh
#   sh ./install.sh                       # latest -> ~/.local/bin/pi
#   sh ./install.sh --prefix "$HOME/bin"  # custom prefix
#   GITHUB_TOKEN=... sh ./install.sh      # private repo

set -eu

INSTALLER_VERSION="0.1.0"
REPO="hakergeniusz/pi-rs"
BINARY="pi"
API_BASE="https://api.github.com/repos/${REPO}"

# Defaults; each can be overridden (see usage()).
PREFIX="${PI_INSTALL_DIR:-$HOME/.local/bin}"
TAG="${PI_VERSION:-latest}"
TOKEN="${GITHUB_TOKEN:-${PI_GITHUB_TOKEN:-}}"
USE_SUDO=0
FORCE=0

die() {
  printf 'install.sh: error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
pi-rs installer ${INSTALLER_VERSION}

Usage: sh install.sh [OPTIONS] [TAG]

Options:
  -h, --help          Show this help and exit.
  -V, --version       Print the installer version and exit.
  -p, --prefix DIR    Install into DIR (default: \$PI_INSTALL_DIR or ~/.local/bin).
      --prefix=DIR    Same, in a single argument.
      --sudo          Use sudo for the mkdir/cp/chmod/mv steps (opt-in only).
      --force         Replace an existing \$PREFIX/$BINARY (default: refuse).
  [TAG]               Release tag to install, e.g. v0.1.0 (default: \$PI_VERSION
                      or "latest").

Environment:
  PI_INSTALL_DIR   Default install prefix.
  PI_VERSION       Default release tag.
  GITHUB_TOKEN     Bearer token for the GitHub API; REQUIRED while the
                   repository is private (needs read access to Releases).
  PI_GITHUB_TOKEN  Alias for GITHUB_TOKEN.
  TMPDIR           Where the temporary download directory is created.

Downloaded files are checksum-verified before installation. No 'curl | sh',
no writes outside the prefix (and a cleaned-up temp dir), no sudo unless you
ask for it.
EOF
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# ---------------------------------------------------------------- arguments
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)    usage; exit 0 ;;
    -V|--version) printf 'pi-rs installer %s\n' "$INSTALLER_VERSION"; exit 0 ;;
    -p|--prefix|--prefix=*)
      case "$1" in
        --prefix=*) PREFIX=${1#--prefix=}; shift ;;
        *) [ "$#" -ge 2 ] || die "--prefix requires a directory argument"
           PREFIX=$2; shift 2 ;;
      esac
      ;;
    --sudo) USE_SUDO=1; shift ;;
    --force) FORCE=1; shift ;;
    --) shift; break ;;
    -*) die "unknown option: $1 (try --help)" ;;
    *) TAG=$1; shift ;;
  esac
done
[ "$#" -eq 0 ] || die "unexpected extra argument: $1 (try --help)"

# ------------------------------------------------------------- host mapping
os=$(uname -s)
case "$os" in
  Linux)  os=linux ;;
  Darwin) os=darwin ;;
  *) die "unsupported operating system: $os (expected Linux or Darwin)" ;;
esac

machine=$(uname -m)
case "$machine" in
  x86_64|amd64)   arch=x86_64 ;;
  aarch64|arm64)  arch=aarch64 ;;
  *) die "unsupported CPU architecture: $machine (expected x86_64 or aarch64)" ;;
esac

asset="${BINARY}-${os}-${arch}"
checksum_asset="${asset}.sha256"

# ------------------------------------------------------------------ tooling
if command -v curl >/dev/null 2>&1; then
  DOWNLOADER=curl
elif command -v wget >/dev/null 2>&1; then
  DOWNLOADER=wget
else
  die "need curl or wget to download the release assets"
fi

TMPDIR_BASE="${TMPDIR:-/tmp}"
WORK=$(mktemp -d "${TMPDIR_BASE%/}/pi-rs-install.XXXXXX") \
  || die "cannot create a temporary directory in $TMPDIR_BASE"
trap 'rm -rf "$WORK"' EXIT HUP INT TERM

bin_tmp="$WORK/$asset"
sum_tmp="$WORK/checksums"

# ----------------------------------------------------------------- network
API_JSON=""

# Fetch the release JSON once (only needed for the private-repo fallback).
get_api_json() {
  [ -n "$API_JSON" ] && return 0
  if [ "$TAG" = latest ]; then
    api_url="${API_BASE}/releases/latest"
  else
    api_url="${API_BASE}/releases/tags/${TAG}"
  fi
  if [ -n "$TOKEN" ]; then
    API_JSON=$(curl -fsSL \
      -H "Accept: application/vnd.github+json" \
      -H "Authorization: Bearer $TOKEN" \
      "$api_url") || die "cannot query GitHub API at $api_url (check GITHUB_TOKEN and repo access)"
  else
    API_JSON=$(curl -fsSL \
      -H "Accept: application/vnd.github+json" \
      "$api_url") || die "cannot query GitHub API at $api_url"
  fi
}

# Print the browser_download_url whose basename equals $1, if any.
url_from_json() {
  wanted=$1
  printf '%s' "$API_JSON" | tr -d '\n' \
    | grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | sed 's/^"browser_download_url"[[:space:]]*:[[:space:]]*"//; s/"$//' \
    | while IFS= read -r u; do
        case "$u" in
          */"$wanted") printf '%s\n' "$u"; break ;;
        esac
      done
}

# download URL -> dest. Returns non-zero on failure so callers can fall back.
download() {
  url=$1 dest=$2
  if [ "$DOWNLOADER" = curl ]; then
    if [ -n "$TOKEN" ]; then
      curl -fsSL -H "Authorization: Bearer $TOKEN" \
        -H "Accept: application/octet-stream" -o "$dest" "$url" || return 1
    else
      curl -fsSL -o "$dest" "$url" || return 1
    fi
  else
    if [ -n "$TOKEN" ]; then
      wget -q --header="Authorization: Bearer $TOKEN" \
        --header="Accept: application/octet-stream" -O "$dest" "$url" || return 1
    else
      wget -q -O "$dest" "$url" || return 1
    fi
  fi
}

# Resolve an asset by its deterministic name. Public repos use the direct
# releases/download URL; private repos need the API (token) fallback.
fetch_asset() {
  filename=$1 dest=$2
  if [ "$TAG" = latest ]; then
    url="https://github.com/${REPO}/releases/latest/download/${filename}"
  else
    url="https://github.com/${REPO}/releases/download/${TAG}/${filename}"
  fi
  if download "$url" "$dest"; then
    return 0
  fi
  if [ -n "$TOKEN" ]; then
    get_api_json
    api_url=$(url_from_json "$filename")
    [ -n "$api_url" ] || return 1
    download "$api_url" "$dest"
  else
    return 1
  fi
}

# ---------------------------------------------------------------- download
printf 'install.sh: fetching %s for %s/%s (release %s)\n' "$BINARY" "$os" "$arch" "$TAG"

fetch_asset "$asset" "$bin_tmp" \
  || die "could not download '$asset' for release '$TAG'. The repository is private: set GITHUB_TOKEN to a token with read access to Releases, or pass a valid TAG."
[ -s "$bin_tmp" ] || die "downloaded binary is empty"

if fetch_asset "$checksum_asset" "$sum_tmp"; then
  :
else
  fetch_asset "SHA256SUMS" "$sum_tmp" \
    || die "could not download a checksum file ('${asset}.sha256' or 'SHA256SUMS')"
fi

# -------------------------------------------------------------- checksum
expected=$(awk -v n="$asset" '{
    name = $2
    sub(/^\*/, "", name)
    if (name == n) { print tolower($1); exit }
  }' "$sum_tmp")
[ -n "$expected" ] \
  || die "no checksum entry for '$asset' in the downloaded checksum file"

if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$bin_tmp" | awk '{print tolower($1)}')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$bin_tmp" | awk '{print tolower($1)}')
else
  die "need sha256sum or 'shasum -a 256' to verify the download"
fi

[ "$actual" = "$expected" ] \
  || die "SHA-256 mismatch for '$asset': expected $expected, got $actual (nothing was installed)"

printf 'install.sh: checksum OK (%s)\n' "$actual"

# ---------------------------------------------------------------- install
if [ -e "${PREFIX}/${BINARY}" ] && [ "$FORCE" -ne 1 ]; then
  die "${PREFIX}/${BINARY} already exists; pass --force to replace it, or choose another --prefix"
fi

if [ "$USE_SUDO" -eq 1 ]; then
  sudo mkdir -p "$PREFIX" || die "sudo mkdir -p $PREFIX failed"
else
  mkdir -p "$PREFIX" || die "cannot create $PREFIX (use --prefix you own, or --sudo)"
  [ -w "$PREFIX" ] || die "$PREFIX is not writable (use --prefix you own, or --sudo)"
fi

target="${PREFIX}/${BINARY}"
staging="${PREFIX}/.${BINARY}.install.$$"
if [ "$USE_SUDO" -eq 1 ]; then
  sudo cp "$bin_tmp" "$staging" || die "failed to stage binary in $PREFIX"
  sudo chmod 0755 "$staging" || die "failed to set executable bit"
  sudo mv -f "$staging" "$target" || die "failed to install $target"
else
  cp "$bin_tmp" "$staging" || die "failed to stage binary in $PREFIX"
  chmod 0755 "$staging" || die "failed to set executable bit"
  mv -f "$staging" "$target" || die "failed to install $target"
fi

# ------------------------------------------------------------ next steps
cat <<EOF

pi-rs ${TAG} installed successfully: ${target}

Next steps:
  1. Make sure the install directory is on your PATH:
       export PATH="${PREFIX}:\$PATH"     # add this line to your shell profile
  2. Check the binary:
       "${target}" --help
       "${target}" -p "Say hello"
  3. Sessions/config live under XDG roots (defaults: ~/.config/pi-rs,
     ~/.local/share/pi-rs). See README.md -> Paths for details.

To replace a future release: sh install.sh --force
To uninstall: remove ${target}
EOF
