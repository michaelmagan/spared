#!/bin/sh
# Test scripts/try.sh without touching any account: dry-run every target, check the templates
# embedded in try.sh match deploy/, and check the no-CLI-found path. Run: sh scripts/test-try.sh
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
try="$root/scripts/try.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/spared-test-try.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
fail() { echo "test-try: FAIL: $*" >&2; exit 1; }

for t in fly railway.json railway.Dockerfile; do
  f=$t
  [ "$t" = fly ] && f=fly.toml
  sh "$try" --print-template "$t" | cmp - "$root/deploy/$f" || fail "embedded $t differs from deploy/$f"
done

# The image try.sh pulls must be one the docker workflow pushes: the release version tag.
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
[ -n "$version" ] || fail "no version in Cargo.toml"
grep -q "^  DEFAULT_IMAGE=ghcr.io/[a-z0-9._/-]*/spared:$version\$" "$try" || fail "try.sh DEFAULT_IMAGE is not tagged with the Cargo.toml version ($version)"
grep -q 'type=semver,pattern={{version}}' "$root/.github/workflows/docker.yml" || fail "docker.yml does not push the version tag try.sh pulls"
if grep -n 'VERI''FY\|PLACE''HOLDER\|INSTALL''_URL' "$try" "$root/scripts/install.sh" "$root"/deploy/*; then fail "unfinished marker in a script or deploy template"; fi

mkdir "$tmp/t"
for t in docker fly railway vm; do
  TMPDIR="$tmp/t" SPARED_APP=spared-test sh "$try" --dry-run --target "$t" --yes >"$tmp/$t.out" 2>&1 || fail "--dry-run --target $t exited nonzero"
  grep -q 'Dry run' "$tmp/$t.out" || fail "$t: dry run banner missing"
done
grep -q '^+ docker run -d --name spared --read-only --tmpfs /tmp:rw,noexec,nosuid,size=64m --user 10001:10001 --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=256 --memory=512m -p 127.0.0.1:8080:8080 -v spared-data:/data ' "$tmp/docker.out" || fail "docker command missing or not bound to 127.0.0.1"
if grep -q 'Warning: --public' "$tmp/docker.out"; then fail "docker: --public warning printed without --public"; fi
TMPDIR="$tmp/t" sh "$try" --dry-run --target docker --yes --public >"$tmp/docker-public.out" 2>&1 || fail "--public exited nonzero"
grep -q '^+ docker run -d --name spared --read-only .* -p 8080:8080 -v spared-data:/data ' "$tmp/docker-public.out" || fail "docker --public: command missing"
grep -q 'Warning: --public publishes port 8080 on every interface' "$tmp/docker-public.out" || fail "docker --public: warning missing"
grep -q '/_/notes/' "$tmp/docker.out" || fail "docker: path form missing"
grep -q '^+ fly apps create spared-test$' "$tmp/fly.out" || fail "fly apps create missing"
grep -q '^+ fly volumes create spared_data --app spared-test --region iad --size 1 --yes$' "$tmp/fly.out" || fail "fly volumes create missing"
grep -q '^+ fly deploy --config .*/fly.toml --app spared-test --ha=false$' "$tmp/fly.out" || fail "fly deploy missing"
grep -q 'path = "/healthz"' "$tmp/fly.out" || fail "fly: health check missing from fly.toml"
grep -q '"healthcheckPath": "/healthz"' "$tmp/railway.out" || fail "railway: health check missing from railway.json"
grep -q '^+ railway init --name spared-test$' "$tmp/railway.out" || fail "railway init missing"
grep -q '^+ railway volume add --mount-path /data$' "$tmp/railway.out" || fail "railway volume add missing"
grep -q 'https://spared-test.fly.dev/_/notes/' "$tmp/fly.out" || fail "fly: URL missing"
grep -q '^+ railway up ' "$tmp/railway.out" || fail "railway up missing"
grep -q 'docker run -d --name spared --restart unless-stopped --read-only .*--cap-drop=ALL --security-opt=no-new-privileges --pids-limit=256 --memory=512m -p 80:8080' "$tmp/vm.out" || fail "vm command missing or not hardened"
grep -q "curl --proto '=https' --tlsv1.2 -fsSL https://get.docker.com" "$tmp/vm.out" || fail "vm: get.docker.com curl not hardened"
for t in docker fly railway vm; do
  grep -q 'serves every app from one browser origin' "$tmp/$t.out" || fail "$t: path routing warning missing"
done
[ -z "$(find "$tmp/t" -mindepth 1)" ] || fail "dry run wrote files"

# no CLI on PATH: --yes must fail with a message that names the fix; vm still works
bin="$tmp/bin"
mkdir "$bin"
for c in sh cat sed od tr date grep ls mktemp rm dirname cmp head tail; do
  p=$(command -v "$c" || true)
  [ -n "$p" ] && ln -s "$p" "$bin/$c"
done
if PATH="$bin" sh "$try" --yes >"$tmp/none.out" 2>&1; then fail "no CLI: expected nonzero exit"; fi
grep -q 'None of railway, fly or docker was found' "$tmp/none.out" || fail "no CLI: message missing"
grep -q 'docs.docker.com' "$tmp/none.out" || fail "no CLI: install hint missing"
if PATH="$bin" sh "$try" --target docker >"$tmp/nodocker.out" 2>&1; then fail "docker missing: expected nonzero exit"; fi
grep -q 'Docker CLI not found' "$tmp/nodocker.out" || fail "docker missing: message missing"
PATH="$bin" sh "$try" --target vm --yes >/dev/null 2>&1 || fail "vm target should not need any CLI"
if sh "$try" --target nope >/dev/null 2>&1; then fail "bad target accepted"; fi
# --help works when the script arrives on stdin (curl | sh -s -- --help), where $0 is not the file
sh -s -- --help <"$try" >"$tmp/help.out" 2>&1 || fail "--help from stdin: nonzero exit"
grep -q '^Options:' "$tmp/help.out" && grep -q 'SPARED_IMAGE' "$tmp/help.out" || fail "--help from stdin: usage not printed"
# no flags and no terminal: usage and exit 2, no shell noise (setsid drops the controlling terminal)
if command -v setsid >/dev/null 2>&1; then
  rc=0
  setsid -w sh "$try" </dev/null >"$tmp/notty.out" 2>"$tmp/notty.err" || rc=$?
  [ "$rc" = 2 ] || fail "no terminal: expected exit 2, got $rc"
  grep -q 'no terminal for the menu: pass --target' "$tmp/notty.err" || fail "no terminal: usage missing"
  if grep -q '/dev/tty' "$tmp/notty.err"; then fail "no terminal: shell noise about /dev/tty"; fi
  if grep -q 'Where do you want to deploy' "$tmp/notty.out"; then fail "no terminal: menu was printed"; fi
else
  echo "test-try: setsid not found; skipping the no-terminal check" >&2
fi
# A truncated download must run nothing: feed the first 1/4, 1/2 and 3/4 of each script (cut at
# every line count) to sh with stub CLIs that leave a marker if they are ever called.
stub="$tmp/stub"
mkdir "$stub"
for c in docker fly flyctl railway curl wget; do
  printf '#!/bin/sh\necho called >>"%s/marker"\n' "$tmp" >"$stub/$c"
  chmod +x "$stub/$c"
done
for script in try.sh install.sh; do
  total=$(wc -l <"$root/scripts/$script")
  for n in 10 $((total / 4)) $((total / 2)) $((total * 3 / 4)) $((total - 3)) $((total - 1)); do
    rm -f "$tmp/marker"
    head -n "$n" "$root/scripts/$script" >"$tmp/part.sh"
    PATH="$stub:$PATH" SPARED_INSTALL_DIR="$tmp/inst-$n" HOME="$tmp" sh "$tmp/part.sh" --target docker --yes >"$tmp/part.out" 2>&1 || true
    [ ! -e "$tmp/marker" ] || fail "$script cut at line $n of $total ran a command"
    [ ! -e "$tmp/inst-$n" ] || fail "$script cut at line $n of $total created the install dir"
    if grep -qE 'spared try-it|fetching|Starting the spared|Found on this machine' "$tmp/part.out"; then fail "$script cut at line $n of $total printed program output"; fi
  done
  tail -n 1 "$root/scripts/$script" | grep -q '^main "\$@"$' || fail "$script: last line is not the main call"
done
echo "test-try: PASS"
