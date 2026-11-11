#!/bin/sh
# Install the spared CLI and server from a GitHub release. `sh install.sh --help` lists the options
# and environment variables.
set -eu

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOT'
Install the spared CLI and server from a GitHub release.

  curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/michaelmagan/spared/main/scripts/install.sh | sh
  curl --proto '=https' --tlsv1.2 -fsSL .../install.sh | sh -s -- --yes     # also run `spared setup` without asking

Options:
  --yes        run `spared setup` after installing, without asking
  --no-setup   do not offer `spared setup`
  --help       show this text

Environment:
  SPARED_VERSION        release to install, e.g. 0.0.1-preview. Default: the latest
                        stable release, or, while there is none, the newest prerelease.
  SPARED_INSTALL_DIR    where to put the binary (default: $HOME/.spared/bin)
  SPARED_REPO           GitHub owner/name to download from (default: michaelmagan/spared)
  SPARED_RELEASE_BASE   directory URL that holds SHA256SUMS and the tarballs directly.
                        Overrides the GitHub URLs; used for testing against a local server.
  SPARED_GITHUB_URL, SPARED_GITHUB_API_URL
                        stand-ins for https://github.com and https://api.github.com, used by
                        scripts/test-install.sh against a local server.

The tarball is checked against the release's SHA256SUMS before anything is installed.
To upgrade, run this script again; to uninstall, delete the installed file (and ~/.spared if
you no longer want the SDK and build cache).

Platforms: linux x86_64 and aarch64, macOS arm64 and x86_64. Only linux x86_64 is tested.
The Linux binaries need glibc 2.35 or newer (no musl build yet).
EOT
}

# The whole script is one function, called on the last line: a download cut short ends inside the
# function, which is a syntax error, so a partial script never runs.
main() {
  REPO=${SPARED_REPO:-michaelmagan/spared}
  INSTALL_DIR=${SPARED_INSTALL_DIR:-${HOME:-}/.spared/bin}
  YES=0
  SETUP=1

  for arg in "$@"; do
    case $arg in
      --yes|-y) YES=1 ;;
      --no-setup) SETUP=0 ;;
      --help|-h) usage; exit 0 ;;
      *) die "unknown option: $arg (try --help)" ;;
    esac
  done

  [ -n "${HOME:-}" ] || [ -n "${SPARED_INSTALL_DIR:-}" ] || die "HOME is not set; set SPARED_INSTALL_DIR"

  os=$(uname -s)
  arch=$(uname -m)
  case "$os/$arch" in
    Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin/arm64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    *) die "no prebuilt spared for $os/$arch; build from source with: cargo build --release -p spared" ;;
  esac

  # Everything is fetched over HTTPS. The overrides for testing against a local server
  # (SPARED_RELEASE_BASE, SPARED_GITHUB_URL, SPARED_GITHUB_API_URL) also allow plain HTTP.
  proto='=https'
  wget_proto=--https-only
  if [ -n "${SPARED_RELEASE_BASE:-}${SPARED_GITHUB_URL:-}${SPARED_GITHUB_API_URL:-}" ]; then
    proto='=http,https'
    wget_proto=
  fi
  if command -v curl >/dev/null 2>&1; then
    fetch() { curl --proto "$proto" --tlsv1.2 -fsSL -o "$2" "$1" 2>/dev/null; }
  elif command -v wget >/dev/null 2>&1; then
    # wget_proto is one flag or empty, so it is split on purpose.
    # shellcheck disable=SC2086
    fetch() { wget -q $wget_proto -O "$2" "$1" 2>/dev/null; }
  else
    die "need curl or wget"
  fi

  if command -v sha256sum >/dev/null 2>&1; then
    sha_of() { sha256sum "$1" | awk '{print $1}'; }
  elif command -v shasum >/dev/null 2>&1; then
    sha_of() { shasum -a 256 "$1" | awk '{print $1}'; }
  else
    die "need sha256sum or shasum to verify the download"
  fi

  github=${SPARED_GITHUB_URL:-https://github.com}
  github=${github%/}
  api=${SPARED_GITHUB_API_URL:-https://api.github.com}
  api=${api%/}
  pin_hint="pin a release with SPARED_VERSION=<version> (they are listed at https://github.com/$REPO/releases), or build from source with: cargo build --release -p spared"

  tmp=$(mktemp -d "${TMPDIR:-/tmp}/spared-install.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT INT TERM

  if [ -n "${SPARED_RELEASE_BASE:-}" ]; then
    base=${SPARED_RELEASE_BASE%/}
    say "fetching $base/SHA256SUMS"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS"
  elif [ -n "${SPARED_VERSION:-}" ]; then
    base=$github/$REPO/releases/download/v${SPARED_VERSION#v}
    say "fetching $base/SHA256SUMS"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS. Is there a release v${SPARED_VERSION#v}? They are listed at https://github.com/$REPO/releases"
  else
    # GitHub's "latest" is the newest release that is not a prerelease. While the project has
    # only prereleases it does not exist, so ask the API for the newest release of any kind.
    base=$github/$REPO/releases/latest/download
    say "fetching $base/SHA256SUMS"
    if ! fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
      say "no stable release found; looking for the newest prerelease"
      fetch "$api/repos/$REPO/releases?per_page=20" "$tmp/releases.json" \
        || die "could not list the releases of $REPO (no network, or GitHub's API rate limit). Fix: $pin_hint"
      # The list is newest first. One field per line, then the first tag that looks like a version.
      tag=$(tr ',' '\n' <"$tmp/releases.json" | sed -n 's/^[[:space:]{[]*"tag_name"[[:space:]]*:[[:space:]]*"\(v[0-9][A-Za-z0-9._+-]*\)".*$/\1/p' | head -n 1)
      [ -n "$tag" ] || die "$REPO has no release yet. Fix: $pin_hint"
      say "installing prerelease ${tag#v} (set SPARED_VERSION to choose another)"
      base=$github/$REPO/releases/download/$tag
      say "fetching $base/SHA256SUMS"
      fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS. Fix: $pin_hint"
    fi
  fi

  # The tarball name carries the version, so find it by target in SHA256SUMS. The name becomes a
  # local path, so it may hold only characters that cannot climb out of $tmp (no slash).
  line=$(grep -E "^[0-9a-fA-F]{64}  spared-[A-Za-z0-9._+-]+-$target\.tar\.gz\$" "$tmp/SHA256SUMS" | head -n 1 || true)
  [ -n "$line" ] || die "SHA256SUMS lists no tarball for $target"
  want=${line%% *}
  file=${line##* }
  case $file in
    */*|*..*) die "SHA256SUMS names a tarball with a path in it ($file); nothing was installed" ;;
  esac

  say "downloading $file"
  fetch "$base/$file" "$tmp/$file" || die "could not download $base/$file"

  got=$(sha_of "$tmp/$file")
  if [ "$got" != "$want" ]; then
    die "checksum mismatch for $file: expected $want, got $got; nothing was installed"
  fi
  say "checksum ok"

  tar -xzf "$tmp/$file" --no-same-owner -C "$tmp" || die "could not unpack $file"
  dir=${file%.tar.gz}
  [ -f "$tmp/$dir/spared" ] && [ ! -L "$tmp/$dir/spared" ] || die "$file does not contain $dir/spared"

  mkdir -p "$INSTALL_DIR"
  cp "$tmp/$dir/spared" "$INSTALL_DIR/spared.new"
  chmod 755 "$INSTALL_DIR/spared.new"
  if ! version=$("$INSTALL_DIR/spared.new" --version 2>&1); then
    rm -f "$INSTALL_DIR/spared.new"
    die "the downloaded spared does not run on this machine: $version
this binary needs glibc 2.35 or newer on Linux (ldd --version shows yours); on an older system, build from source: cargo build --release -p spared
nothing was installed"
  fi
  mv -f "$INSTALL_DIR/spared.new" "$INSTALL_DIR/spared"
  say "installed $INSTALL_DIR/spared ($version)"

  case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      say ""
      say "$INSTALL_DIR is not on your PATH. Add it:"
      say "  export PATH=\"$INSTALL_DIR:\$PATH\""
      say "and put that line in your shell profile (~/.profile, ~/.bashrc or ~/.zshrc)."
      ;;
  esac

  [ "$SETUP" = 1 ] || return 0

  say ""
  say "spared setup installs the wasm32-wasip2 Rust target and wasi-sdk 25 into ~/.spared (needed to build apps)."
  run_setup=0
  if [ "$YES" = 1 ]; then
    run_setup=1
  elif ( : </dev/tty >/dev/tty ) 2>/dev/null; then
    printf 'Run it now? [y/N] ' >/dev/tty
    answer=
    read -r answer </dev/tty || answer=
    case $answer in y|Y|yes|YES) run_setup=1 ;; esac
  fi

  if [ "$run_setup" = 1 ]; then
    "$INSTALL_DIR/spared" setup
  else
    say "Run it later with: $INSTALL_DIR/spared setup"
  fi
}

main "$@"
