# Experimental deploy paths

Docker is the tested path. Fly.io and Railway are scripted in `scripts/try.sh` but have not been run (see [run the examples from the image](operating.md#run-the-examples-from-the-image)). The paths below are experimental. They were written from each vendor's documentation and have not been run against the real provider, so expect to adjust them.

Every block uses the same image, `ghcr.io/michaelmagan/spared`, at the release tag `0.0.1-preview` (the tag scheme is in `.github/workflows/docker.yml`, and `scripts/try.sh` names the same tag). Change it to the release you want.

Rules that hold for every target:

- Never publish the admin listener. It stays on `127.0.0.1:8081` inside the container and no block maps it.
- The admin token is generated on the server into `/data/admin-token` on first start. Read it there and do not paste it into a config file.
- TLS is yours to add. The VPS blocks bind the app port to loopback and leave TLS to a proxy such as the Caddy file in `deploy/Caddyfile`.
- The image turns on path routing, so every app on the server shares one browser origin. Do not run apps you do not trust, see [the FAQ](faq.md).

## Any machine with Docker

```sh
docker run -d --name spared --restart unless-stopped --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=64m --user 10001:10001 \
  --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=256 --memory=512m \
  -p 127.0.0.1:8080:8080 -v spared-data:/data \
  ghcr.io/michaelmagan/spared:0.0.1-preview
curl http://localhost:8080/healthz
docker exec spared cat /data/admin-token
```

## Any Ubuntu or Debian VPS

`deploy/cloud-init.yaml` installs Docker from the distribution and runs the command above. Paste it into the provider's user-data field when you create the server. On an existing machine, run the `docker run` block above after `apt-get install -y docker.io`.

To look at the server before adding TLS, tunnel to it:

```sh
ssh -L 8080:127.0.0.1:8080 root@<server-ip>
```

For a public name, point a DNS record at the machine, install Caddy, put the hostname into `deploy/Caddyfile` and load it. Apps then answer at `https://<hostname>/_/<app>/`.

## Deploy an app to a remote server

The admin listener stays on the server's loopback, so reach it through an SSH tunnel and deploy as if the server were local. This works for a `spared serve` you started on the machine itself. The Docker commands above keep the admin listener inside the container, where the host's loopback cannot reach it.

```sh
ssh -L 8081:127.0.0.1:8081 -L 8080:127.0.0.1:8080 <user>@<server-ip>   # leave this session open
```

In a second terminal on your own machine, with the app's directory as the working directory:

```sh
export SPARED_TOKEN=$(ssh <user>@<server-ip> cat <data-dir>/admin-token)
spared deploy
curl http://<app>.localhost:8080/
```

`<data-dir>` is the directory the remote `spared serve` was started with. The default admin address, `http://127.0.0.1:8081`, is now the tunnel, so `spared deploy`, `ps` and `logs` need no `--server`. Port 8081 on your machine must be free, which means no local `spared serve` or `spared dev` at the same time. The build runs on your machine, so it needs `spared setup` there, not on the server. Never publish the admin port.

## Hetzner Cloud

```sh
hcloud server create --name spared --type <server-type> --image ubuntu-24.04 \
  --user-data-from-file deploy/cloud-init.yaml
```

Pick a small type from `hcloud server-type list`, choose a location, and add `--ssh-key <name>` so you can log in. Follow the VPS steps above for the tunnel and TLS.

## DigitalOcean

```sh
doctl compute droplet create spared --image ubuntu-24-04-x64 --size <size> \
  --region <region> --user-data-file deploy/cloud-init.yaml --ssh-keys <key-id-or-fingerprint>
```

Pick a small size from `doctl compute size list`. Follow the VPS steps above for the tunnel and TLS.

## Render

`deploy/render.yaml` is a blueprint for one web service from the image with a persistent disk at `/data`. Copy it to `render.yaml` in a repository you own, then create a Blueprint in the Render dashboard from that repository. Render terminates TLS and fills in `PORT`, which the image reads. A persistent disk needs a paid instance type on Render, so check that before you create it. Read the admin token from the service shell with `cat /data/admin-token`.

## Coolify, Dokploy and similar platforms

Create an application from a Docker image and enter:

| Field | Value |
| --- | --- |
| Image | `ghcr.io/michaelmagan/spared` |
| Tag | `0.0.1-preview` |
| Port | `8080` |
| Volume | a persistent volume mounted at `/data` |
| Health check path | `/healthz` |
| Environment | none required (`PORT` if the platform sets one) |

Do not add a port mapping or a domain for 8081. Set the container to run as user `10001:10001` with a read-only root filesystem and a writable `/tmp` if the platform exposes those options. The platform's proxy provides TLS. The admin token is in `/data/admin-token`, readable from the platform's terminal for the container.

## Not yet run against the real provider

None of these were run against the real provider: the cloud-init file, the Hetzner Cloud command, the DigitalOcean command, the Render blueprint, the Caddy file and the Coolify and Dokploy settings. The Docker command is the one the main docs already use.
