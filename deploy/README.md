# Parley server — deploy kit

One container, one UDP port, one volume. No database service, no proxy.

```sh
docker load -i parley-image-<version>.tar.gz
cp .env.example .env          # set PARLEY_ADMIN_PASSWORD
docker compose up -d
```

The first boot creates the `admin` account from
`PARLEY_ADMIN_PASSWORD`. Add everyone else (the password is the last
argument, quote it):

```sh
docker compose exec parley /parley-server adduser alice 'a long password'
```

Then remove `PARLEY_ADMIN_PASSWORD` from `.env` — it only applies while
the users table is empty.

## Ports and firewall

`PARLEY_PORT` (default 443) is the host UDP port; the container always
listens on 4443. QUIC is UDP only — no TCP rule is needed.

```sh
sudo ufw allow 443/udp
```

Clients connect with `your-server:443` (or the port you chose).

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
docker load -i parley-image-<new-version>.tar.gz
docker compose up -d
```

The volume keeps accounts, messages and the certificate.

## Checks

```sh
docker compose logs -f          # "parley listening addr=… fingerprint=…"
docker compose exec parley /parley-server --help
```
