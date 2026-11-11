#!/bin/sh
# Test scripts/install.sh against a local web server that stands in for GitHub: which release it
# picks (stable, the newest prerelease while there is no stable one, or a pinned version), that it
# refuses a tarball whose checksum does not match, and what it says when there is no release.
# Touches no network and nothing outside a temp dir. Run: sh scripts/test-install.sh
# Needs python3 (for the web server), tar, and sha256sum or shasum.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
install="$root/scripts/install.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/spared-test-install.XXXXXX")
server_pid=
# shellcheck disable=SC2329  # invoked by the EXIT trap
cleanup() {
  if [ -n "$server_pid" ]; then kill "$server_pid" 2>/dev/null || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT
fail() { echo "test-install: FAIL: $*" >&2; exit 1; }

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) echo "test-install: no release target for this platform; skipping" >&2; exit 0 ;;
esac
if command -v sha256sum >/dev/null 2>&1; then sum() { sha256sum "$@"; }; else sum() { shasum -a 256 "$@"; }; fi

repo=acme/spared
www="$tmp/www"
api_dir="$www/api/repos/$repo"
mkdir -p "$api_dir"

# make_release <dir under $www> <version>: a tarball whose `spared` prints its version, plus
# SHA256SUMS, laid out and named the way .github/workflows/release.yml publishes them.
make_release() {
  dir="$www/$1"
  name="spared-$2-$target"
  mkdir -p "$dir" "$tmp/build/$name"
  printf '#!/bin/sh\necho "spared %s (test build)"\n' "$2" >"$tmp/build/$name/spared"
  for f in LICENSE NOTICE THIRD-PARTY-NOTICES; do echo "$f of the test build" >"$tmp/build/$name/$f"; done
  chmod 755 "$tmp/build/$name/spared"
  tar -C "$tmp/build" -czf "$dir/$name.tar.gz" "$name"
  (cd "$dir" && sum "$name.tar.gz" >SHA256SUMS)
}

port=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
python3 -I -m http.server "$port" --bind 127.0.0.1 --directory "$www" >"$tmp/http.log" 2>&1 &
server_pid=$!
n=0
until python3 -I -c 'import socket, sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=1).close()' "$port" 2>/dev/null; do
  n=$((n + 1))
  [ "$n" -lt 50 ] || fail "the local web server did not start"
  sleep 0.1
done

# run <install dir> [VAR=value ...]: install.sh against the local server; output in $tmp/out.
run() {
  dest=$1
  shift
  env HOME="$tmp/home" SPARED_REPO="$repo" SPARED_INSTALL_DIR="$dest" \
    SPARED_GITHUB_URL="http://127.0.0.1:$port" SPARED_GITHUB_API_URL="http://127.0.0.1:$port/api" \
    "$@" sh "$install" --no-setup >"$tmp/out" 2>&1
}

# 1. Only a prerelease exists (launch day): GitHub's "latest" is a 404, so the newest release in
#    the API list is installed.
make_release "$repo/releases/download/v0.0.1-preview" 0.0.1-preview
make_release "$repo/releases/download/v0.0.1-alpha" 0.0.1-alpha
cat >"$api_dir/releases" <<'EOF'
[
  {
    "tag_name": "v0.0.1-preview",
    "name": "v0.0.1-preview",
    "draft": false,
    "prerelease": true
  },
  {
    "tag_name": "v0.0.1-alpha",
    "prerelease": true
  }
]
EOF
run "$tmp/bin1" || { cat "$tmp/out" >&2; fail "prerelease only: install exited nonzero"; }
[ "$("$tmp/bin1/spared" --version)" = "spared 0.0.1-preview (test build)" ] || fail "prerelease only: wrong binary installed"
grep -q 'installing prerelease 0.0.1-preview' "$tmp/out" || fail "prerelease only: did not say which prerelease"
grep -q 'checksum ok' "$tmp/out" || fail "prerelease only: checksum not verified"
grep -q 'is not on your PATH' "$tmp/out" || fail "prerelease only: no PATH hint"

# The same list as one line of JSON, as an API or a proxy may send it.
printf '[{"url":"x","tag_name":"v0.0.1-preview","prerelease":true},{"tag_name":"v0.0.1-alpha"}]' >"$api_dir/releases"
run "$tmp/bin1b" || { cat "$tmp/out" >&2; fail "minified list: install exited nonzero"; }
[ "$("$tmp/bin1b/spared" --version)" = "spared 0.0.1-preview (test build)" ] || fail "minified list: wrong binary installed"

# 2. A pinned version wins over everything.
run "$tmp/bin2" SPARED_VERSION=0.0.1-alpha || { cat "$tmp/out" >&2; fail "pinned: install exited nonzero"; }
[ "$("$tmp/bin2/spared" --version)" = "spared 0.0.1-alpha (test build)" ] || fail "pinned: wrong binary installed"
run "$tmp/bin2v" SPARED_VERSION=v0.0.1-alpha || fail "pinned with a leading v: install exited nonzero"
if run "$tmp/bin2x" SPARED_VERSION=9.9.9; then fail "pinned to a missing release: expected nonzero exit"; fi
grep -q 'Is there a release v9.9.9' "$tmp/out" || fail "pinned to a missing release: message does not name it"
[ ! -e "$tmp/bin2x" ] || fail "pinned to a missing release: created the install dir"

# 3. A wrong checksum installs nothing and leaves an existing binary alone.
cp -R "$www/$repo/releases/download/v0.0.1-alpha" "$www/$repo/releases/download/v0.0.2-bad"
sed "s/^[0-9a-f]\{64\}/$(printf '%064d' 0)/; s/0\.0\.1-alpha/0.0.2-bad/" "$www/$repo/releases/download/v0.0.1-alpha/SHA256SUMS" >"$www/$repo/releases/download/v0.0.2-bad/SHA256SUMS"
mv "$www/$repo/releases/download/v0.0.2-bad/spared-0.0.1-alpha-$target.tar.gz" "$www/$repo/releases/download/v0.0.2-bad/spared-0.0.2-bad-$target.tar.gz"
if run "$tmp/bin2" SPARED_VERSION=0.0.2-bad; then fail "bad checksum: expected nonzero exit"; fi
grep -q 'checksum mismatch' "$tmp/out" && grep -q 'nothing was installed' "$tmp/out" || { cat "$tmp/out" >&2; fail "bad checksum: message missing"; }
[ "$("$tmp/bin2/spared" --version)" = "spared 0.0.1-alpha (test build)" ] || fail "bad checksum: the installed binary was replaced"
[ ! -e "$tmp/bin2/spared.new" ] || fail "bad checksum: left a partial file"

# A binary that cannot run on this machine (an older glibc) stops the install with the reason and
# leaves the existing binary in place.
make_release "$repo/releases/download/v0.0.3-broken" 0.0.3-broken
broken="$www/$repo/releases/download/v0.0.3-broken"
printf '#!/bin/sh\necho "spared: version GLIBC_2.35 not found" >&2\nexit 1\n' >"$tmp/build/spared-0.0.3-broken-$target/spared"
tar -C "$tmp/build" -czf "$broken/spared-0.0.3-broken-$target.tar.gz" "spared-0.0.3-broken-$target"
(cd "$broken" && sum "spared-0.0.3-broken-$target.tar.gz" >SHA256SUMS)
if run "$tmp/bin2" SPARED_VERSION=0.0.3-broken; then fail "binary that does not run: expected nonzero exit"; fi
grep -q 'does not run on this machine' "$tmp/out" && grep -q 'glibc 2.35 or newer' "$tmp/out" && grep -q 'GLIBC_2.35 not found' "$tmp/out" || { cat "$tmp/out" >&2; fail "binary that does not run: message missing"; }
[ "$("$tmp/bin2/spared" --version)" = "spared 0.0.1-alpha (test build)" ] || fail "binary that does not run: the installed binary was replaced"
[ ! -e "$tmp/bin2/spared.new" ] || fail "binary that does not run: left a partial file"

# 4. Once a stable release exists it is preferred over a newer prerelease.
make_release "$repo/releases/latest/download" 0.1.0
run "$tmp/bin4" || { cat "$tmp/out" >&2; fail "stable: install exited nonzero"; }
[ "$("$tmp/bin4/spared" --version)" = "spared 0.1.0 (test build)" ] || fail "stable: wrong binary installed"
if grep -q 'prerelease' "$tmp/out"; then fail "stable: looked for a prerelease"; fi
rm -r "$www/$repo/releases/latest"

# 5. No release at all, and an API that cannot be reached: both name the fix.
printf '[]\n' >"$api_dir/releases"
if run "$tmp/bin5"; then fail "no release: expected nonzero exit"; fi
grep -q 'has no release yet' "$tmp/out" && grep -q 'SPARED_VERSION=' "$tmp/out" && grep -q 'cargo build --release -p spared' "$tmp/out" || { cat "$tmp/out" >&2; fail "no release: message does not name the fix"; }
rm "$api_dir/releases"
if run "$tmp/bin5"; then fail "API unreachable: expected nonzero exit"; fi
grep -q 'could not list the releases' "$tmp/out" && grep -q 'SPARED_VERSION=' "$tmp/out" || { cat "$tmp/out" >&2; fail "API unreachable: message does not name the fix"; }
[ ! -e "$tmp/bin5" ] || fail "no release: created the install dir"

# 6. The asset names the installer looks for are the ones the release workflow produces.
grep -q 'name="spared-\$version-\$TARGET"' "$root/.github/workflows/release.yml" || fail "release.yml no longer names tarballs spared-<version>-<target>"
grep -q 'SHA256SUMS' "$root/.github/workflows/release.yml" || fail "release.yml no longer publishes SHA256SUMS"
for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin x86_64-apple-darwin; do
  grep -q "target=$t" "$install" || fail "install.sh does not know target $t"
  grep -q "target: $t" "$root/.github/workflows/release.yml" || fail "release.yml does not build target $t"
done

# 7. --help works when the script arrives on stdin (curl | sh -s -- --help), where $0 is not the file.
sh -s -- --help <"$install" >"$tmp/out" 2>&1 || fail "--help from stdin: nonzero exit"
grep -q '^Options:' "$tmp/out" && grep -q 'SPARED_INSTALL_DIR' "$tmp/out" || { cat "$tmp/out" >&2; fail "--help from stdin: usage not printed"; }

echo "test-install: PASS"
