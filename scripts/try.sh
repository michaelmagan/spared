#!/bin/sh
# Try spared in about two minutes: deploy the server image with three example apps. `sh try.sh --help`
# lists the options, environment variables and what each target does.
set -eu


say() { printf '%s\n' "$*"; }

# The image turns on path routing (/_/<app>/) so the bundled examples answer on one hostname.
path_warning() {
  say ""
  say "Warning: the /_/<app>/ path form serves every app from one browser origin, so any app's pages"
  say "can read and change every other app's data. The image turns it on for the bundled examples only."
  say "For your own apps, use one host per app (wildcard DNS plus SPARED_DOMAIN) and set SPARED_PATH_ROUTING=0."
}
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

tpl_fly() {
  cat <<'EOT'
# Fly app configuration for the spared server image.
# Rendered by scripts/try.sh with the app name and image filled in. Hosted platforms give one
# hostname, so apps are reached by path: https://<app-name>.fly.dev/_/<spared app>/ .
app = "__APP__"
primary_region = "iad"

[build]
  image = "__IMAGE__"

[env]
  PORT = "8080"

[http_service]
  internal_port = 8080
  force_https = true
  auto_stop_machines = "stop"
  auto_start_machines = true
  min_machines_running = 0

[[http_service.checks]]
  grace_period = "30s"
  interval = "30s"
  method = "GET"
  timeout = "5s"
  path = "/healthz"

[mounts]
  source = "spared_data"
  destination = "/data"

[[vm]]
  size = "shared-cpu-1x"
  memory = "512mb"
EOT
}

tpl_railway_json() {
  cat <<'EOT'
{
  "$schema": "https://railway.com/railway.schema.json",
  "build": {
    "builder": "DOCKERFILE",
    "dockerfilePath": "Dockerfile"
  },
  "deploy": {
    "healthcheckPath": "/healthz",
    "healthcheckTimeout": 120,
    "restartPolicyType": "ON_FAILURE",
    "restartPolicyMaxRetries": 3
  }
}
EOT
}

tpl_railway_dockerfile() {
  cat <<'EOT'
# Railway builds this one-line Dockerfile, so the service runs the published spared image.
# Rendered by scripts/try.sh as the Dockerfile next to railway.json, with the image filled in.
FROM __IMAGE__
EOT
}

usage() {
  cat <<'EOT'
Try spared in about two minutes: deploy the server image with three example apps.

  curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/michaelmagan/spared/main/scripts/try.sh | sh
  sh scripts/try.sh --dry-run            # print every command, change nothing
  sh scripts/try.sh --target docker --yes

It detects the railway, fly (or flyctl) and docker CLIs, shows a menu of what it found plus
"show me the commands for a VM", and runs the steps for your choice.

Options:
  --dry-run          print every command and every file it would write; run nothing
  --yes, -y          do not ask; with no --target, use the first of docker, fly, railway found
  --target T         railway | fly | docker | vm; skips the menu
  --public           docker target: publish the port on every interface of this machine, not
                     only on 127.0.0.1 (the examples accept anonymous writes; prints a warning)
  --help, -h         show this text

Environment:
  SPARED_IMAGE       image to deploy (default: DEFAULT_IMAGE below, the release this script
                     belongs to)
  SPARED_APP         app name for fly and railway (default: spared-<random>)
  SPARED_PORT        local port for the docker target (default: 8080)

The image turns on path routing (/_/<app>/, env SPARED_PATH_ROUTING) so the examples answer on
one hostname. That puts every app on one browser origin: fine for the bundled examples, not for
apps you do not trust (see docs/faq.md). Every target prints this warning with its URLs.

The image is built and pushed by .github/workflows/docker.yml, which tags it with the release
version; DEFAULT_IMAGE below names that tag (scripts/test-try.sh checks the two agree).

The Fly and Railway commands and config keys follow the vendors' CLI and configuration
references (fly apps create, fly volumes create, fly deploy, fly.toml; railway init, add,
volume add, up, domain, railway.json). The config templates are deploy/fly.toml,
deploy/railway.json and deploy/railway.Dockerfile; they are embedded here so this script works
piped from curl. scripts/test-try.sh checks they match.
EOT
}

# Container hardening shared by the docker and vm commands. Verified against the server image: it
# starts, the three examples answer and accept writes. The root filesystem is read-only (the app
# data lives in the volume, scratch space in the tmpfs), the process runs as the image's own
# unprivileged user so it needs no capability, and it cannot gain any. Memory is the 512 MB the
# Fly template uses.
HARDEN="--read-only --tmpfs /tmp:rw,noexec,nosuid,size=64m --user 10001:10001 --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=256 --memory=512m"

have() { command -v "$1" >/dev/null 2>&1; }

# Commands are printed with a leading "+ ". Under --dry-run they are not run.
run() {
  printf '+ %s\n' "$*"
  if [ "$DRY" = 0 ]; then "$@"; fi
}

# True when /dev/tty can really be opened; the subshell keeps the shell's own open error quiet.
have_tty() { [ -r /dev/tty ] && ( : </dev/tty ) 2>/dev/null; }

# Read one line from the terminal, even when this script itself is piped from curl.
ask() {
  prompt=$1
  if have_tty; then
    printf '%s' "$prompt" >/dev/tty
    IFS= read -r ANSWER </dev/tty || ANSWER=
  else
    die "no terminal to ask on. Pass --target railway|fly|docker|vm and --yes, or run with --dry-run to see the plan."
  fi
}

confirm_hosted() {
  [ "$YES" = 1 ] && return 0
  [ "$DRY" = 1 ] && return 0
  say ""
  say "This creates a new app and a small persistent volume in your $1 account. That can cost money,"
  say "and the app and volume stay until you delete them."
  ask "Continue? [y/N] "
  case $ANSWER in y|Y|yes|YES) ;; *) die "stopped; nothing was created" ;; esac
}

need_cli() {
  # need_cli <binary-or-empty> <name> <install hint>
  [ "$DRY" = 1 ] && return 0
  [ -n "$1" ] && return 0
  die "$2 CLI not found. Install it ($3), or choose another target with --target docker|fly|railway|vm."
}

rand_suffix() {
  if [ -r /dev/urandom ]; then
    od -An -N3 -tx1 /dev/urandom | tr -d ' \n'
  else
    date +%s | tail -c 7
  fi
}


print_urls_local() {
  say ""
  say "Three example apps are running. Open any of these:"
  say "  http://notes.localhost:$PORT_LOCAL/        a notes page"
  say "  http://list.localhost:$PORT_LOCAL/items    a JSON list (POST {\"text\":\"milk\"} to add)"
  say "  http://demo.localhost:$PORT_LOCAL/         a demo page"
  say "If *.localhost does not resolve on your system, use the path form on any host:"
  say "  http://localhost:$PORT_LOCAL/_/notes/"
  say "  http://localhost:$PORT_LOCAL/_/list/items"
  say "  http://localhost:$PORT_LOCAL/_/demo/"
  path_warning
  say "Stop it with: docker rm -f spared   (data stays in the spared-data volume)"
}

wait_ready() {
  # wait_ready <url>: first start compiles the examples, which takes a few seconds.
  if [ "$DRY" = 1 ]; then
    say "(would wait up to 90 s for $1 to answer)"
    return 0
  fi
  have curl || { say "curl not found; skipping the readiness check"; return 0; }
  n=0
  while [ "$n" -lt 90 ]; do
    if curl --proto '=http,https' -fsS -o /dev/null --max-time 3 "$1" 2>/dev/null; then return 0; fi
    n=$((n + 1))
    sleep 1
  done
  return 1
}

do_docker() {
  need_cli "$( [ "$HAVE_DOCKER" = 1 ] && echo docker )" Docker "https://docs.docker.com/get-docker/"
  say "Starting the spared image with Docker (admin API stays inside the container)."
  if [ "$DRY" = 0 ] && docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx spared; then
    die "a container named 'spared' already exists. Fix: docker rm -f spared   (its data stays in the spared-data volume)"
  fi
  # Loopback only by default: the examples accept anonymous writes, and Docker's port rules bypass
  # host firewalls, so publishing on every interface would serve them to the whole network.
  if [ "$PUBLIC" = 1 ]; then
    say "Warning: --public publishes port $PORT_LOCAL on every interface of this machine. Anyone who can"
    say "reach it can use the example apps, which accept anonymous writes. Docker's rules bypass most host firewalls."
    bind="$PORT_LOCAL:8080"
  else
    bind="127.0.0.1:$PORT_LOCAL:8080"
  fi
  # HARDEN is a fixed word list without spaces inside a word, so it is split on purpose.
  # shellcheck disable=SC2086
  run docker run -d --name spared $HARDEN -p "$bind" -v spared-data:/data "$IMAGE"
  if ! wait_ready "http://127.0.0.1:$PORT_LOCAL/healthz" || ! wait_ready "http://127.0.0.1:$PORT_LOCAL/_/notes/"; then
    say "The server did not answer in 90 s. Look at: docker logs spared"
    exit 1
  fi
  print_urls_local
}

do_vm() {
  say "On any VM with Docker and a public port:"
  say ""
  say "  # 1. install Docker if the VM has none"
  say "  curl --proto '=https' --tlsv1.2 -fsSL https://get.docker.com | sh"
  say ""
  say "  # 2. run the image on port 80; data lives in the spared-data volume"
  say "  docker run -d --name spared --restart unless-stopped $HARDEN -p 80:8080 -v spared-data:/data $IMAGE"
  say ""
  say "  # 3. try it (from your machine; VM_IP is the VM's public address)"
  say "  curl http://VM_IP/_/notes/"
  say "  curl http://VM_IP/_/list/items"
  say "  curl http://VM_IP/_/demo/"
  path_warning
  say ""
  say "With wildcard DNS (*.apps.example.com pointing at the VM), also pass -e SPARED_DOMAIN=apps.example.com"
  say "and use http://notes.apps.example.com/ . The server speaks plain HTTP: put a TLS proxy such as"
  say "Caddy in front before real use. Do not publish port 8081; the admin API deploys code."
  say ""
  say "Over SSH in one go: ssh user@VM_IP 'docker run -d --name spared --restart unless-stopped $HARDEN -p 80:8080 -v spared-data:/data $IMAGE'"
}

do_fly() {
  need_cli "$FLY_BIN" Fly "https://fly.io/docs/flyctl/install/"
  fly_cmd=${FLY_BIN:-fly}
  confirm_hosted Fly
  say "Deploying $IMAGE to Fly as app '$APP'."
  if [ "$DRY" = 1 ]; then
    dir=${TMPDIR:-/tmp}/spared-fly.XXXXXX
    say "would write $dir/fly.toml:"
    tpl_fly | sed "s|__APP__|$APP|g; s|__IMAGE__|$IMAGE|g; s/^/    /"
  else
    dir=$(mktemp -d "${TMPDIR:-/tmp}/spared-fly.XXXXXX")
    tpl_fly | sed "s|__APP__|$APP|g; s|__IMAGE__|$IMAGE|g" >"$dir/fly.toml"
    say "wrote $dir/fly.toml"
  fi
  if [ "$DRY" = 0 ] && ! "$fly_cmd" auth whoami >/dev/null 2>&1; then
    say "You are not logged in to Fly."
    run "$fly_cmd" auth login
  elif [ "$DRY" = 1 ]; then
    say "(would run '$fly_cmd auth login' if '$fly_cmd auth whoami' fails)"
  fi
  run "$fly_cmd" apps create "$APP"
  run "$fly_cmd" volumes create spared_data --app "$APP" --region iad --size 1 --yes
  run "$fly_cmd" deploy --config "$dir/fly.toml" --app "$APP" --ha=false
  say ""
  say "Fly gives the app one hostname, so apps are reached by path:"
  say "  https://$APP.fly.dev/_/notes/"
  say "  https://$APP.fly.dev/_/list/items"
  say "  https://$APP.fly.dev/_/demo/"
  path_warning
  say "If that does not answer, find the hostname with: $fly_cmd status --app $APP"
  say "Remove everything with: $fly_cmd apps destroy $APP --yes   (this deletes the volume and its data)"
}

do_railway() {
  need_cli "$( [ "$HAVE_RAILWAY" = 1 ] && echo railway )" Railway "https://docs.railway.com/guides/cli"
  confirm_hosted Railway
  say "Deploying $IMAGE to Railway as '$APP'."
  if [ "$DRY" = 1 ]; then
    dir=${TMPDIR:-/tmp}/spared-railway.XXXXXX
    say "would write $dir/Dockerfile:"
    tpl_railway_dockerfile | sed "s|__IMAGE__|$IMAGE|g; s/^/    /"
    say "would write $dir/railway.json:"
    tpl_railway_json | sed 's/^/    /'
  else
    dir=$(mktemp -d "${TMPDIR:-/tmp}/spared-railway.XXXXXX")
    tpl_railway_dockerfile | sed "s|__IMAGE__|$IMAGE|g" >"$dir/Dockerfile"
    tpl_railway_json >"$dir/railway.json"
    say "wrote $dir/Dockerfile and $dir/railway.json"
  fi
  if [ "$DRY" = 0 ] && ! railway whoami >/dev/null 2>&1; then
    say "You are not logged in to Railway."
    run railway login
  elif [ "$DRY" = 1 ]; then
    say "(would run 'railway login' if 'railway whoami' fails)"
  fi
  if [ "$DRY" = 0 ]; then cd "$dir"; else say "+ cd $dir"; fi
  run railway init --name "$APP"
  run railway add --service "$APP"
  run railway volume add --mount-path /data
  run railway up --detach --service "$APP"
  run railway domain --service "$APP"
  say ""
  say "Railway gives the service one hostname, so apps are reached by path. The 'railway domain'"
  say "output above is that hostname (HOST); if it printed none, open the project in the dashboard."
  say "  https://HOST/_/notes/"
  say "  https://HOST/_/list/items"
  say "  https://HOST/_/demo/"
  path_warning
  say "The first deploy takes a minute or two. Remove it with: railway down, or delete the project in the dashboard."
}

# The executable part is one function, called on the last line: a download cut short ends inside
# it (a syntax error) or before it, so a partial script never runs anything.
main() {
  DEFAULT_IMAGE=ghcr.io/michaelmagan/spared:0.0.1-preview
  IMAGE=${SPARED_IMAGE:-$DEFAULT_IMAGE}
  PORT_LOCAL=${SPARED_PORT:-8080}
  DRY=0
  YES=0
  PUBLIC=0
  TARGET=

  while [ $# -gt 0 ]; do
    case $1 in
      --dry-run) DRY=1 ;;
      --yes|-y) YES=1 ;;
      --public) PUBLIC=1 ;;
      --target)
        [ $# -ge 2 ] || die "--target needs a value: railway, fly, docker or vm"
        TARGET=$2
        shift
        ;;
      --target=*) TARGET=${1#--target=} ;;
      --print-template)
        [ $# -ge 2 ] || die "--print-template needs fly, railway.json or railway.Dockerfile"
        case $2 in
          fly) tpl_fly ;;
          railway.json) tpl_railway_json ;;
          railway.Dockerfile) tpl_railway_dockerfile ;;
          *) die "unknown template: $2" ;;
        esac
        exit 0
        ;;
      --help|-h) usage; exit 0 ;;
      *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
  done

  case $TARGET in
    ""|railway|fly|docker|vm) ;;
    *) die "unknown target '$TARGET': use railway, fly, docker or vm" ;;
  esac

  FLY_BIN=
  if have fly; then FLY_BIN=fly; elif have flyctl; then FLY_BIN=flyctl; fi
  HAVE_RAILWAY=0
  if have railway; then HAVE_RAILWAY=1; fi
  HAVE_DOCKER=0
  if have docker; then HAVE_DOCKER=1; fi

  APP=${SPARED_APP:-spared-$(rand_suffix)}

  say "spared try-it: deploys the server image with three example apps (notes, list, demo)."
  say "No Rust toolchain is needed. Image: $IMAGE"
  if [ "$DRY" = 1 ]; then say "Dry run: nothing is executed or changed."; fi
  say ""
  say "Found on this machine:"
  say "  railway CLI: $( [ "$HAVE_RAILWAY" = 1 ] && echo yes || echo no )"
  say "  fly CLI:     $( [ -n "$FLY_BIN" ] && echo "yes ($FLY_BIN)" || echo no )"
  say "  docker:      $( [ "$HAVE_DOCKER" = 1 ] && echo yes || echo no )"

  if [ -z "$TARGET" ]; then
    opts=
    [ "$HAVE_RAILWAY" = 1 ] && opts="$opts railway"
    [ -n "$FLY_BIN" ] && opts="$opts fly"
    [ "$HAVE_DOCKER" = 1 ] && opts="$opts docker"
    if [ -z "$opts" ]; then
      say ""
      say "None of railway, fly or docker was found. Pick one:"
      say "  - Docker (run it here):      https://docs.docker.com/get-docker/ , then run this script again"
      say "  - Fly:                       https://fly.io/docs/flyctl/install/"
      say "  - Railway:                   https://docs.railway.com/guides/cli"
      say "  - Any VM with Docker:        sh try.sh --target vm   (prints the commands)"
      if [ "$YES" = 1 ]; then exit 1; fi
    fi
    if [ "$YES" = 1 ]; then
      case " $opts " in
        *" docker "*) TARGET=docker ;;
        *" fly "*) TARGET=fly ;;
        *" railway "*) TARGET=railway ;;
      esac
    else
      if ! have_tty; then
        if [ "$DRY" = 1 ]; then
          say "(dry run without a terminal: use --target to print a target's commands)"
          exit 0
        fi
        printf '%s\n' "no terminal for the menu: pass --target railway|fly|docker|vm and --yes, or --dry-run --target ..." >&2
        exit 2
      fi
      say ""
      say "Where do you want to deploy?"
      i=1
      for o in $opts; do
        case $o in
          railway) label="Railway (hosted, uses your Railway account)" ;;
          fly) label="Fly (hosted, uses your Fly account)" ;;
          docker) label="Docker, on this machine" ;;
        esac
        say "  $i) $label"
        eval "choice_$i=\$o"
        i=$((i + 1))
      done
      say "  $i) Show me the commands for a VM (nothing is run)"
      vm_n=$i
      ask "Choose [1-$vm_n]: "
      case $ANSWER in
        ''|*[!0-9]*) die "not a number: '$ANSWER'" ;;
      esac
      if [ "$ANSWER" -eq "$vm_n" ]; then
        TARGET=vm
      elif [ "$ANSWER" -ge 1 ] && [ "$ANSWER" -lt "$vm_n" ]; then
        eval "TARGET=\$choice_$ANSWER"
      else
        die "choose a number from 1 to $vm_n"
      fi
    fi
  fi

  say ""
  case $TARGET in
    docker) do_docker ;;
    fly) do_fly ;;
    railway) do_railway ;;
    vm) do_vm ;;
    *) die "nothing to do" ;;
  esac
}

main "$@"
