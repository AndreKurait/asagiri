#!/bin/sh
# Install a verified Asagiri app or configure the signed Amazon Linux RPM repo.
# Parse the complete script before starting: a truncated pipe cannot execute
# only part of an installation.
main() {
set -eu

site=https://andrekurait.github.io/asagiri
key_sha256=446a7526cf15a012eef684b349c93549c8225b61805384294681df4634abce96
channel=latest
start=yes
dry_run=no
while [ "$#" -gt 0 ]; do
  case "$1" in
    --channel)
      [ "$#" -ge 2 ] || { echo "--channel needs latest, stable, or preview" >&2; exit 2; }
      channel=$2; shift ;;
    --no-start) start=no ;;
    --dry-run) dry_run=yes ;;
    --help)
      echo "Usage: install.sh [--channel latest|stable|preview] [--no-start] [--dry-run]"
      echo "latest selects stable when available, otherwise the newest tested prerelease."
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done
case "$channel" in latest|stable|preview) ;; *) echo "Invalid release channel" >&2; exit 2 ;; esac

fail() { echo "Asagiri: $*" >&2; exit 1; }
fetch() { curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --retry 3 "$1" -o "$2"; }
hash() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1
  else shasum -a 256 "$1" | cut -d ' ' -f 1; fi
}
elevate() {
  if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi
}
command -v curl >/dev/null 2>&1 || fail "curl is required"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/asagiri-install.XXXXXXXX")
stage='' target=''
cleanup() {
  if [ -n "$stage" ] && [ -d "$stage" ]; then
    if [ -e "$stage/previous.app" ] && [ ! -e "$target" ]; then
      mv "$stage/previous.app" "$target" || {
        echo "Previous app retained at $stage/previous.app" >&2
        rm -rf "$tmp"
        return
      }
    fi
    rm -rf "$stage"
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
arch=$(uname -m)

case "$(uname -s)" in
  Darwin)
    case "$arch" in arm64|x86_64) ;; *) fail "Unsupported Mac architecture: $arch" ;; esac
    # A terminal running under Rosetta still needs the native Apple Silicon app.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then arch=arm64; fi
    major=$(sw_vers -productVersion | cut -d . -f 1)
    [ "$major" -ge 14 ] || fail "macOS 14 or newer is required"
    fetch "$site/downloads/$channel/macos-$arch.txt" "$tmp/metadata" ||
      fail "This channel has no published download yet. See $site/install/"
    version='' url='' expected=''
    while IFS=' ' read -r key value; do
      case "$key" in
        version) [ -z "$version" ] || fail "Duplicate version"; version=$value ;;
        url) [ -z "$url" ] || fail "Duplicate URL"; url=$value ;;
        sha256) [ -z "$expected" ] || fail "Duplicate digest"; expected=$value ;;
        *) fail "Invalid download metadata" ;;
      esac
    done < "$tmp/metadata"
    printf '%s\n' "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-dev\.[1-9][0-9]*)?$' ||
      fail "Invalid release version"
    printf '%s\n' "$expected" | grep -Eq '^[a-f0-9]{64}$' || fail "Invalid download digest"
    [ "$url" = "https://github.com/AndreKurait/asagiri/releases/download/v$version/Asagiri-$version-$arch.tar.gz" ] ||
      fail "The download URL does not identify an Asagiri release"
    appdir=$HOME/Applications
    [ ! -w /Applications ] || appdir=/Applications
    target=$appdir/Asagiri.app
    echo "Asagiri $version for macOS $arch → $target"
    [ "$dry_run" != yes ] || exit 0
    if [ -e "$target" ]; then
      [ "$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$target/Contents/Info.plist" 2>/dev/null)" = dev.asagiri.app ] ||
        fail "$target is not an Asagiri app"
      ! pgrep -x asagiri >/dev/null 2>&1 || fail "Quit Asagiri before updating; its container data will be retained"
    fi
    fetch "$url" "$tmp/app.tar.gz"
    [ "$(hash "$tmp/app.tar.gz")" = "$expected" ] || fail "The app download failed its SHA-256 check"
    tar -xzf "$tmp/app.tar.gz" -C "$tmp"
    [ "$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$tmp/Asagiri.app/Contents/Info.plist")" = dev.asagiri.app ] ||
      fail "The archive contains a different app"
    /usr/bin/codesign --verify --deep --strict "$tmp/Asagiri.app"
    mkdir -p "$appdir"
    stage=$(mktemp -d "$appdir/.asagiri-install.XXXXXXXX")
    /usr/bin/ditto "$tmp/Asagiri.app" "$stage/Asagiri.app"
    if [ -e "$target" ]; then mv "$target" "$stage/previous.app"; fi
    if mv "$stage/Asagiri.app" "$target"; then
      rm -rf "$stage"
    else
      [ ! -e "$stage/previous.app" ] || mv "$stage/previous.app" "$target"
      fail "Could not install the app; the previous installation was restored"
    fi
    echo "Installed. Open Asagiri from Applications, or run:"
    printf '  open "%s"\n' "$target"
    echo "If macOS asks for approval, use System Settings → Privacy & Security → Open Anyway."
    ;;
  Linux)
    case "$arch" in x86_64|aarch64) ;; *) fail "Unsupported Linux architecture: $arch" ;; esac
    [ -r /etc/os-release ] || fail "Cannot identify this Linux distribution"
    # shellcheck disable=SC1091
    . /etc/os-release
    [ "${ID:-}" = amzn ] && [ "${VERSION_ID:-}" = 2023 ] ||
      fail "This setup command supports Amazon Linux 2023. Other Linux packages: $site/install/"
    command -v dnf >/dev/null 2>&1 || fail "DNF is required"
    echo "Configure the signed Asagiri $channel RPM repository for Amazon Linux 2023 $arch."
    [ "$start" != yes ] || echo "First setup starts the engine and gives your account container access (equivalent to root access)."
    [ "$dry_run" != yes ] || exit 0
    fetch "$site/rpm/$channel/$arch/repodata/repomd.xml" "$tmp/repomd.xml" ||
      fail "This channel's package repository is not published yet. See $site/install/"
    fetch "$site/keys/asagiri-rpm.asc" "$tmp/key.asc"
    [ "$(hash "$tmp/key.asc")" = "$key_sha256" ] || fail "RPM signing key verification failed"
    cat > "$tmp/asagiri.repo" <<EOF
# Managed by the Asagiri installer.
[asagiri]
name=Asagiri ($channel)
baseurl=$site/rpm/$channel/\$basearch
enabled=1
gpgcheck=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-asagiri
metadata_expire=15m
EOF
    if [ -e /etc/yum.repos.d/asagiri.repo ]; then
      grep -q '^# Managed by the Asagiri installer\.$' /etc/yum.repos.d/asagiri.repo ||
        fail "An existing custom /etc/yum.repos.d/asagiri.repo needs your review before replacement"
    fi
    elevate install -D -m 0644 "$tmp/key.asc" /etc/pki/rpm-gpg/RPM-GPG-KEY-asagiri
    elevate rpm --import /etc/pki/rpm-gpg/RPM-GPG-KEY-asagiri
    elevate install -D -m 0644 "$tmp/asagiri.repo" /etc/yum.repos.d/asagiri.repo
    elevate dnf --refresh -y install asagiri
    if [ "$start" = yes ]; then
      [ -d /run/systemd/system ] || fail "Installed. Start it on a systemd host, or use --no-start when preparing an image."
      setup_user=${SUDO_USER:-$(id -un)}
      result=0
      elevate /usr/bin/asagiri host-access "$setup_user" || result=$?
      case "$result" in 0) ;; 10) echo "Log out and back in for container access." ;; *) exit "$result" ;; esac
      cli=/usr/libexec/asagiri/bin/docker
      as_user() {
        if [ "$(id -un)" = "$setup_user" ]; then "$@"
        else sudo -H -u "$setup_user" -- "$@"; fi
      }
      if ! as_user "$cli" context inspect asagiri >/dev/null 2>&1; then
        as_user "$cli" context create asagiri --docker host=unix:///run/asagiri/docker.sock
      fi
      echo "Ready. Try: docker --context asagiri run --rm alpine echo hello"
    fi
    echo "Updates: sudo dnf --refresh upgrade asagiri"
    echo "An upgrade keeps running containers in place. Restart the engine when convenient to use the new binary."
    ;;
  *) fail "This installer supports macOS and Amazon Linux 2023" ;;
esac
}

main "$@"
