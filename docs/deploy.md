# Deploy the server

One container, one UDP port, one volume. The image is a static musl
binary on `scratch` — no shell, no libc, ~10 MB.

## 1. Prerequisites

- A machine (VPS or spare box) with Docker and the compose plugin.
- **UDP** port reachable from the internet. All traffic is QUIC over
  UDP; a TCP-only firewall will not work. A domain name is optional —
  clients accept a host name or an IP.
- About 60 MB of RAM idle (measured 54.8 MB at info logging; the
  container is 1.4 MiB).

```sh
sudo ufw allow 4443/udp     # or your chosen port
```

## 2. Install

From the release page, download `parley-deploy-<version>.tar.gz` — it
contains the compose file, the env template, the runbook and the server
image. Then, on the server:

```sh
tar xf parley-deploy-<version>.tar.gz
cd parley-deploy-<version>
docker load -i parley-image-<version>.tar.gz

cp .env.example .env
# edit .env: set PARLEY_ADMIN_PASSWORD (first boot only), keep PARLEY_PORT=4443

docker compose up -d
```

The first boot creates the `admin` account from `PARLEY_ADMIN_PASSWORD`.
Watch for the identity line:

```sh
docker compose logs -f parley
# INFO parley_server::server: parley listening addr=0.0.0.0:4443 fingerprint=<64 hex>
```

The fingerprint is permanent (it lives in the volume). Send it to the
team with the address — the client shows what it pinned on first
connect, and comparing the two is the whole trust model.

Now remove `PARLEY_ADMIN_PASSWORD` from `.env`; it only applies while
the users table is empty.

## 3. Accounts

There is no user-management UI yet. Create each account inside the
running container:

```sh
docker compose exec parley /parley-server adduser alice 'a long password'
docker compose exec parley /parley-server adduser bob 'another password'
```

A `BUSY` error means the write raced the server's own connection —
retry. Names and passwords are case-sensitive.

## 4. Verify from outside

Download `parley-cli-<version>-linux-x86_64.tar.gz` on any machine, or
use a checkout (`tools/vps-check.sh`):

```sh
tar xf parley-cli-<version>-linux-x86_64.tar.gz
./parley-cli-<version>-linux-x86_64/parley-cli \
    --server your-server --user alice --password 'a long password' --check
# CHECK OK server=your-server:4443 user=alice id=2 channels=1 rtt_ms=…
```

`--check` does QUIC + TLS + TOFU + password auth + channel list and
exits 0. It opens no audio or video devices, so it works on any machine.

## 5. Hand the client to the team

Download `parley-<version>-linux-x86_64.tar.gz` on each machine:

```sh
tar xf parley-<version>-linux-x86_64.tar.gz
cd parley-<version>-linux-x86_64
./install.sh          # installs to ~/.local, checks runtime deps
```

Then launch "Parley", connect to `your-server`, log in. Details and
troubleshooting: [client.md](client.md).

## Upgrades

```sh
tar xf parley-deploy-<new>.tar.gz && cd parley-deploy-<new>
docker load -i parley-image-<new>.tar.gz
docker compose up -d
```

The volume keeps accounts, history and the certificate — the
fingerprint does not change, so nobody has to re-pin.

## Backup and restore

```sh
docker run --rm -v parley-data:/data -v "$PWD":/backup busybox \
    tar czf /backup/parley-data-$(date +%F).tgz -C /data .
```

Restore into a fresh volume with `tar xzf` the same way (stop the
container first). Everything the server knows is in that volume.

## Troubleshooting

| symptom | cause / fix |
|:--|:--|
| client hangs on "connect" | UDP port not open, wrong port, or a TCP-only firewall rule |
| every client refuses the connection: certificate changed | the volume was recreated (new identity), or a stale pin. Clients show both fingerprints and ask before trusting; only accept it if the new one matches this container's startup log. From a shell, the pinned line lives in `~/.config/parley/known_hosts` |
| `no users and PARLEY_ADMIN_PASSWORD not set` | first boot without the variable and an empty volume. Set it and restart once, or `adduser` |
| `adduser` prints BUSY | server was writing; run it again |
| container restarts in a loop | port already bound on the host; change `PARLEY_PORT` |
| `docker compose up` cannot find `parley:latest` | the image was not loaded (`docker load`) and `PARLEY_IMAGE` points elsewhere |

## Building the image from source instead

```sh
git clone https://github.com/DartLumix/Parley.git && cd Parley
docker build -t parley:local .
docker compose up -d          # the root compose file builds and runs
```

The musl binary can also be built directly:
`cargo build --release --target x86_64-unknown-linux-musl -p parley-server`
(needs the `x86_64-unknown-linux-musl` target and `musl-tools`).
