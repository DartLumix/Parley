# Parley server — deploy kit

One container, two UDP ports, one volume. No database service, no proxy.

```sh
cd parley-deploy-<version>          # this directory
docker load -i parley-image-<version>.tar.gz
cp .env.example .env          # set PARLEY_ADMIN_PASSWORD
docker compose up -d
```

The first boot creates the `admin` account from
`PARLEY_ADMIN_PASSWORD`. From there, connect as `admin` and use **Server
management** in the app to create accounts, set tiers and manage
channels. For scripted setup the CLI still works (the password is the
last argument, quote it):

```sh
docker compose exec parley /parley-server adduser alice 'a long password'
```

Then remove `PARLEY_ADMIN_PASSWORD` from `.env` — it only applies while
the users table is empty.

## Ports and firewall

`PARLEY_PORT` (default 4443) is the control/voice UDP port and
`PARLEY_MOQ_PORT` (default 4445) carries screen, camera and share audio
over MoQ. The container listens on both. All traffic is UDP — no TCP
rule is needed.

```sh
sudo ufw allow 4443/udp
sudo ufw allow 4445/udp
```

Clients connect with `your-server` (the client default is 4443), or
`your-server:<port>` if you chose something else. The MoQ port is
announced by the server in the authenticated handshake, so clients do
not need it in the address; it must still be reachable.

## Screen sharing

The server is a zero-transcode MoQ relay. It forwards each publisher's
H.264 video and Opus share audio to the room and never decodes or
re-encodes a frame, so a GPU-less VPS is enough. The publisher adapts
its own bitrate and, only under sustained encoder pressure, a small
resolution tier; see [docs/deploy.md](../docs/deploy.md).

## Identity

The certificate and key live in the `parley-data` volume. The
fingerprint printed at startup is permanent; share it with the team so
they can compare it on first connect. Deleting the volume regenerates
the identity and every client will refuse to reconnect until its pin is
removed.

## Backup

```sh
docker run --rm -v parley-data:/data -v "$PWD":/backup busybox \
    tar czf /backup/parley-data-$(date +%F).tgz -C /data .
```

Restore into a fresh volume the same way with `tar xzf`.

## Upgrade

```sh
tar xf parley-deploy-<new-version>.tar.gz
cd parley-deploy-<new-version>
docker load -i parley-image-<new-version>.tar.gz
docker compose up -d
```

The volume keeps accounts, messages and the certificate.

## Checks

```sh
docker compose logs -f          # "parley listening addr=… fingerprint=…"
docker compose exec parley /parley-server --help
```
