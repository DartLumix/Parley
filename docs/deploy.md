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

Connect as `admin` and open **Server management** (the shield in the
header) to add, edit and remove accounts, set each account's tier, and
create, rename or delete channels. The tiers are `user` (ordinary chat,
voice and shares), `mod` (the same management page, but no permission
changes and no power over admins) and `admin` (everything). The last
admin cannot be removed or demoted.

**Kick** drops somebody from their voice channel immediately; it is not
a ban — they can re-join at once.

The container CLI remains for bootstrap and scripted setup (it creates
an ordinary user):

```sh
docker compose exec parley /parley-server adduser alice 'a long password'
docker compose exec parley /parley-server adduser bob 'another password'
```

A `BUSY` error means the write raced the server's own connection —
retry. Names and passwords are case-sensitive.

The management page needs a role-aware server and this client. The wire
additions (`Role` on the auth reply, `ManageRequest`/`ManageResult`) are
additive: an older client still works against a new server, and a new
client connected to an older server simply has no management entry
point.

## 4. Screen-share transcode

A viewer that cannot decode the sharer's original stream (a low
`viewer_fps` in its five-second report) can be given a server-encoded
tier instead. The sharer's stream is untouched: capable viewers keep
1080p/60, the struggling viewer gets 720p or 480p at 30 fps. The
server pays one worker thread per active share, and only while a
viewer is below the threshold.

It is on by default. The defaults are one concurrent transcode,
720p then 480p, and one encoder thread:

```toml
[video]
transcode = true
max_transcodes = 1
min_viewer_fps = 24
first_height = 720
second_height = 480
threads = 1
```

`max_transcodes` is host-wide, not per channel and is the CPU/memory
ceiling: one job holds the decoded frame plus one encoder per active
tier (roughly 30–60 MB at 1080p). A second share that needs help falls
back to the sharer's own resolution ladder. To stay a pure relay (no
openh264 work at all), set `transcode = false`. The
container's `/config.toml` is baked; mount your own file over it to
change these values:

```yaml
    volumes:
      - ./config.toml:/config.toml:ro
```

A mounted config overrides the image's baked one; `PARLEY_PRESENCE_LISTEN`
in the environment still wins, so the presence endpoint keeps working
(see below).

## 5. Read-only presence API (dashboards)

Port 4443 is **QUIC over UDP**, not HTTP — `curl` and browsers cannot
speak to it, and container logs are not an interface. For a dashboard on
the same host the server also runs a deliberately small HTTP listener:

- `GET /api/presence` (query strings are ignored; other methods get
  `405`, other paths `404`).
- Default bind `127.0.0.1:4444` (`presence_listen` in `config.toml`; an
  empty value disables it).
- In the container it binds `0.0.0.0:4444` and compose publishes it to
  `127.0.0.1:4444` on the host, so it is not internet-reachable.
- Optional auth: when `PARLEY_PRESENCE_TOKEN` is set, the request must
  carry `Authorization: Bearer <token>` (`401` otherwise). Without the
  variable, loopback access is the only gate.

It is read-only by construction (no handler path writes anything) and
cheap to poll every 5 s: one indexed SQLite `SELECT` for the channel
list plus short in-memory reads of the session and voice tables. The
response is `Cache-Control: no-store` and carries no CORS headers — a
browser page on another origin must call it through its own backend.

```sh
curl -s http://127.0.0.1:4444/api/presence | jq .
# with a token:
curl -s -H "Authorization: Bearer $PARLEY_PRESENCE_TOKEN" \
     http://127.0.0.1:4444/api/presence
```

### Snapshot contract

One object with three keys. Channel and user ids are the stable SQLite
primary keys, prefixed (`ch-1`, `u-7`); they survive reconnects and
restarts.

| field | type | notes |
|:--|:--|:--|
| `server.name` | string | always `"Parley"` |
| `server.user_count` | number | connected accounts, not connections |
| `channels[].id` | string | `ch-<id>` |
| `channels[].name` | string | |
| `channels[].parent_id` | null | always null: Parley has no channel tree |
| `channels[].position` | number | 0-based; the array is already in order |
| `channels[].type` | string | always `"voice"`: every channel is voice + chat |
| `channels[].user_limit` | number | always `0` (no limit) |
| `channels[].mode` | string | `"speech"` or `"music"` (Parley extension) |
| `users[].id` | string | `u-<id>` |
| `users[].display_name` | string | same as `username`; there is no separate display name |
| `users[].username` | string | |
| `users[].avatar_url` | null | Parley has no avatars |
| `users[].channel_id` | string \| null | voice channel; `null` = online, not in a channel |
| `users[].muted` | bool | self-muted microphone, announced by the client |
| `users[].deafened` | bool | always `false` (see below) |
| `users[].afk` | bool | same as `afk_message != null` |
| `users[].afk_message` | string \| null | away text, trimmed, at most 120 chars |
| `users[].screen_sharing` | bool | live share in their current channel |
| `users[].is_bot` | bool | always `false`; Parley has no bots |
| `users[].role` | string | `user`, `mod` or `admin` (Parley extension) |

Semantics to encode in the UI:

- **Only connected users appear**, ordered by user id; a disconnect
  removes the row. `channel_id: null` means authenticated but not in a
  voice channel — there is no separate `online` flag and no offline rows.
- **Channels are flat.** No categories, so `parent_id` is always null and
  everything renders at depth 0. There are no text-only channels: every
  channel carries chat and voice, hence `type` is always `"voice"`.
- **Muted is self-mute.** The server never mutes anyone. **Deafen is not
  observable**: the app's mute-all switch acts on the local mixer only
  and is never sent, so `deafened` is a contract placeholder that never
  becomes true. Do not light a deafen badge from it.
- **AFK is a status, not a channel.** Any connected user can set the away
  text; it survives channel switches and is cleared on sign-out. It is
  free text, nullable.
- **Screen sharing is per user**, not per channel — true for a user with a
  live share, cleared on stop, leave or disconnect. There is no stream
  title or viewer count.
- **One voice channel per user.** A second login for the same account
  replaces the first session, so an account appears at most once.
- **No hidden or locked channels/users**: everything listed is visible to
  every authenticated client.

```json
{
  "server": { "name": "Parley", "user_count": 3 },
  "channels": [
    { "id": "ch-1", "name": "Lobby", "parent_id": null, "position": 0,
      "type": "voice", "user_limit": 0, "mode": "speech" },
    { "id": "ch-2", "name": "Music", "parent_id": null, "position": 1,
      "type": "voice", "user_limit": 0, "mode": "music" }
  ],
  "users": [
    { "id": "u-1", "display_name": "alice", "username": "alice",
      "avatar_url": null, "channel_id": null, "muted": false,
      "deafened": false, "afk": true, "afk_message": "Coming back later…",
      "screen_sharing": false, "is_bot": false, "role": "admin" },
    { "id": "u-2", "display_name": "bob", "username": "bob",
      "avatar_url": null, "channel_id": "ch-1", "muted": true,
      "deafened": false, "afk": false, "afk_message": null,
      "screen_sharing": false, "is_bot": false, "role": "user" },
    { "id": "u-3", "display_name": "carol", "username": "carol",
      "avatar_url": null, "channel_id": "ch-2", "muted": false,
      "deafened": false, "afk": true, "afk_message": "Back in 10",
      "screen_sharing": true, "is_bot": false, "role": "mod" }
  ]
}
```

This is a server-only feature: no client or protocol change. It ships in
the server image; `docker compose up -d` after loading the new image (or
`cargo run -p parley-server` from a checkout).

## 6. Verify from outside

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

## 7. Hand the client to the team

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
(needs the `x86_64-unknown-linux-musl` target, `musl-tools` **and** a
musl C++ toolchain with the static `libstdc++` — openh264 is linked
into the server). The image itself builds on Alpine, which provides
that toolchain natively.
