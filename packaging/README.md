# Strata packaging

Production install layer for the three roles: **sender** (Orange Pi 5,
aarch64), **receiver** (x86_64 cloud), **control** (x86_64 cloud, usually
Docker).

## Layout

```
packaging/
├── install.sh                  # role installer (sender|receiver|control)
├── systemd/                    # units installed to /etc/systemd/system/
│   ├── strata-sender.service
│   ├── strata-receiver.service
│   └── strata-control.service
├── env/                        # examples installed to /etc/strata/<role>.env
│   ├── sender.env.example
│   ├── receiver.env.example
│   └── control.env.example
└── caddy/Caddyfile.example     # TLS reverse proxy for the control plane
docker-compose.prod.yml         # (repo root) postgres + strata-control stack
```

## Installing a role — three commands per box

Build a dist directory with **plain-named** binaries (release assets carry
version/arch suffixes — strip them), e.g. for a sender:

```
dist/
├── install.sh          # this repo's packaging/ tree (or just install.sh
├── systemd/…  env/…    #  alongside systemd/ and env/)
├── strata-sender
├── strata-pipeline
└── libgststrata.so
```

Then on the box:

```bash
scp -r dist/ user@box:
sudo ./dist/install.sh sender        # or receiver / control
sudoedit /etc/strata/sender.env      # set control URL + enrollment token
sudo systemctl enable --now strata-sender
```

The installer is idempotent: it creates the `strata` system user, installs
binaries to `/usr/local/bin`, drops `libgststrata.so` into the multiarch
GStreamer plugin dir (`/usr/lib/<triplet>/gstreamer-1.0`, falling back to
`/usr/local/lib/gstreamer-1.0` + a `GST_PLUGIN_PATH` hint), installs the
unit, and **never overwrites an existing `/etc/strata/<role>.env`**.

### Capabilities — do not setcap

`strata-pipeline` needs `CAP_NET_RAW` (SO_BINDTODEVICE onto modem
interfaces). The sender unit grants it via `AmbientCapabilities=CAP_NET_RAW`,
which child processes inherit. Do **not** `setcap` the binary on
service-managed boxes: the unit sets `NoNewPrivileges=true`, which blocks
file-capability elevation (ambient caps remain fine). `setcap` is only for
running `strata-pipeline` by hand outside systemd.

## Configuration

Env files live in `/etc/strata/` (`sender.env`, `receiver.env`,
`control.env`), root-owned mode 0600. Sender/receiver flags go in
`STRATA_SENDER_ARGS` / `STRATA_RECEIVER_ARGS`; the control plane is
configured purely by environment variables (`DATABASE_URL`, `LISTEN_ADDR`,
`JWT_SEED_B64`, `METRICS_TOKEN`, `CORS_ALLOWED_ORIGINS`). Generate the JWT
seed with `head -c32 /dev/urandom | base64`.

## Control plane via Docker (recommended)

```bash
cat > .env <<EOF
POSTGRES_PASSWORD=$(head -c16 /dev/urandom | base64 | tr -d '/+=')
JWT_SEED_B64=$(head -c32 /dev/urandom | base64)
EOF
docker compose -f docker-compose.prod.yml up -d --build
```

This binds `127.0.0.1:3000` only — TLS terminates at a reverse proxy.

## TLS

Copy `caddy/Caddyfile.example` to `/etc/caddy/Caddyfile`, set your hostname,
`systemctl reload caddy`. Caddy handles Let's Encrypt issuance/renewal and
proxies WebSockets (`/agent/ws`, `/receiver/ws`, `/ws`) out of the box.

nginx alternative: `certbot --nginx` plus a `proxy_pass
http://127.0.0.1:3000` block — remember the explicit `Upgrade`/`Connection
"upgrade"` headers for the WebSocket paths (details in the Caddyfile header).

## Updating

`install.sh` also installs `/usr/local/bin/strata-update.sh` — a pull-based
updater against GitHub Releases (checksum-verified, atomic swap, refuses
while a stream is live, restarts the unit):

```bash
sudo strata-update.sh sender                     # latest
sudo strata-update.sh receiver --version v0.7.0  # pin / roll back
```

Unattended: copy `systemd/strata-update.{service,timer}` in, set `ROLE=`,
`systemctl enable --now strata-update.timer`. Full story:
`wiki/Updates-and-Releases.md`.

## Logs & status

```bash
journalctl -u strata-sender -f      # live logs (any role: -u strata-<role>)
systemctl status strata-sender
```

Units restart automatically (`Restart=always`, 5 s backoff, no start-limit
give-up) — a field sender keeps retrying through long network outages.
