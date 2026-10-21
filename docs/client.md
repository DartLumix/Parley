# The desktop client (Linux)

## Install

Download `parley-<version>-linux-x86_64.tar.gz` from the release page:

```sh
tar xf parley-<version>-linux-x86_64.tar.gz
cd parley-<version>-linux-x86_64
./install.sh
```

That copies the bundle to `~/.local/opt/parley`, links
`~/.local/bin/parley`, and installs a desktop entry and icon so the app
appears in launchers as **Parley**. It refuses to continue, naming the
packages, when a shared library is missing.

`PREFIX=/opt ./install.sh` installs elsewhere; `./install.sh --uninstall`
removes everything again.

Runtime requirements: GTK 3, PipeWire (`libpipewire-0.3`), ALSA
(`libasound`), a C++ runtime and a Wayland or X11 session. The bundle
is built on Ubuntu 24.04, so it needs **glibc 2.39** — Ubuntu 24.04+,
Debian 13+, Fedora 40+ — and a PipeWire new enough to carry
`spa_video_info_raw.flags` (0.3.65+, which 24.04 and everything newer
ships; Ubuntu 22.04's 0.3.48 cannot run this build).
Screen sharing additionally needs a ScreenCast portal backend matching
the desktop (`xdg-desktop-portal-hyprland`, `-wlr`, `-kde`, `-gnome`).

## Connect

    Server:   host or IP, port optional — the default is 4443, so
              "your-server" is enough. The deploy kit publishes 4443
              by default; "your-server:443" only if you changed
              PARLEY_PORT.
    Username: created by the server owner (`docker compose exec parley
              /parley-server adduser alice 'password'`)
    Password: as created.

The first connection pins the server's certificate — the same TOFU
model as SSH. Compare the fingerprint shown after login with the one
the server printed at startup.

If the server's identity changes (a redeploy onto an empty volume, a
restored backup without the certificate), the next login is refused and
the app shows **both** fingerprints — the saved one and the one the
server presents now — with a *Trust new certificate* button. Nothing is
written until you accept, and accepting is only right after the new
fingerprint matches the one the server logs at startup. The pin is
keyed by `host:port`, so `your-server` and `your-server:4443` are the
same trust decision.

## Use

- **Voice** — click *Join* on a channel: joining opens the microphone
  and speakers (TeamSpeak shape, no separate listen toggle). *Leave*
  closes them. The channel card lists who is in voice.
- **Layout** — channels fill the left pane, chat sits on the right. A
  screen share opens above the chat and pushes it down.
- **Chat** — the chat panel belongs to the joined channel; history
  loads on join.
- **Stays signed in** — closing the window is not signing out: the app
  keeps the server-issued session token in
  `~/.config/parley/session.toml` (owner-only, never the password) and
  reconnects to the last server by itself on the next launch. After a
  dropped connection the login screen offers *Reconnect as …*, which uses
  the same token. *Disconnect* is the explicit sign-out and deletes it; an
  expired token drops back to the form with the server and account
  prefilled.
- **Mute** — the mic button beside *Share screen* (available while in
  voice). A muted mic sends no audio at all, and the mute survives
  switching channels. Everyone else's roster shows a muted-mic badge as
  soon as you press it: the state is announced, not guessed from silence.
- **Speaking** — a name lights up in its channel while that person's audio
  is non-silent (measured from the decoded stream, ~10 Hz), and the mic
  button lights with your own capture level. Quiet people stay dark; a
  turned-down person still lights up, because the meter is taken before
  your volume is applied.
- **Volume** — click a name in a channel's roster for their voice slider
  (0–200%). A screen sharer also gets a *Screen share audio* slider there
  and in the share pane: that stream is trimmed separately, so their voice
  keeps its level.
- **Screen share** — *Share screen* in the floating bar asks for a
  resolution and frame-rate cap first (defaults from Settings, *Source*
  means the capture's own size and rate — caps never upscale), then the
  desktop's portal asks which monitor or window. Everyone in the same
  channel gets a card; *Watch* renders it, and the share's system audio
  starts only once you are watching.
- **Your own stream** — while sharing, the share pane shows what you are
  sending, straight from the capture (before encoding), with a switch to
  turn it off. Watching someone else at the same time moves it to a small
  overlay in the corner so it never disappears. Turning it off stops the
  scaling work as well as the picture.
- **Settings** — the gear in the app bar: video (own-stream preview,
  hardware encoding, default share resolution and frame rate), audio
  (echo cancellation, which applies to the next channel you join), and
  account (the connected server and account, plus *Sign out*). Every
  control saves immediately to `~/.config/parley/settings.toml`; a
  preference that only takes effect at encoder/AEC creation says so on
  the page.
- **Fullscreen** — the expand button on a watched share. It is real
  window fullscreen: the compositor's own shortcuts work, Esc exits.
  Nothing is drawn over the picture except two fading controls.
- **Window bar** — the window has no title bar. Move the pointer to the
  very top edge and a thin toolbar slides down with minimise, maximise
  and close; it hides again after a moment. On tiling compositors a
  drag can be done with the compositor's own binding (e.g. Mod+drag on
  Hyprland) — the toolbar's drag area works where the protocol allows
  it.
- **Hardware encoding** — the switch in Settings → Video, shown only in
  builds with the `hw-encode` feature and usable only where VAAPI works.
  It applies the next time an encoder is created (the next share); a
  share in flight keeps the backend it started with.

## Troubleshooting

| symptom | meaning / fix |
|:--|:--|
| `Missing shared libraries` from install.sh | install the packages it names |
| "Server certificate changed" prompt | the pin no longer matches. Compare the fingerprint it shows against the server's startup log; *Trust new certificate* only if they agree |
| connect fails immediately | wrong port (remember it is UDP), or the server's firewall |
| join a channel, no sound | check the system mixer; joining opens the default devices |
| share asks nothing / `portal` error | install the ScreenCast backend for your desktop and restart `xdg-desktop-portal` |
| a watched share is black | sender side: the share's `encoder ready` line; receiver side: frames arriving but not decoding means keyframe/stream shape |
| hardware encoding switch missing | the build lacks `hw-encode`, or VAAPI has no encoder (`vainfo`) |
| share starts but looks blocky after picking a cap | the cap scales the capture down on the CPU; pick *Source* or a larger height on a slow machine |
| own preview stays black | frames are only produced once capture delivers its first buffer — check the `share: encoder ready` line |

Logs: run `~/.local/opt/parley/parley` from a terminal to see
`video:`/`voice:`/`window:` lines; the login page shows the build
version.
