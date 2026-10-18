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
the server printed at startup. To re-pin after the server's identity
was regenerated, delete that server's line from
`~/.config/parley/known_hosts` and reconnect.

## Use

- **Voice** — click *Join* on a channel: joining opens the microphone
  and speakers (TeamSpeak shape, no separate listen toggle). *Leave*
  closes them. The channel card lists who is in voice.
- **Layout** — channels fill the left pane, chat sits on the right. A
  screen share opens above the chat and pushes it down.
- **Chat** — the chat panel belongs to the joined channel; history
  loads on join.
- **Mute** — the mic button beside *Share screen* (available while in
  voice). A muted mic sends no audio at all, and the mute survives
  switching channels.
- **Screen share** — *Share screen* in the floating bar; the desktop's
  portal asks which monitor or window. Everyone in the same channel
  gets a card; *Watch* renders it, and the share's system audio starts
  only once you are watching.
- **Fullscreen** — the expand button on a watched share. It is real
  window fullscreen: the compositor's own shortcuts work, Esc exits.
  Nothing is drawn over the picture except two fading controls.
- **Window bar** — the window has no title bar. Move the pointer to the
  very top edge and a thin toolbar slides down with minimise, maximise
  and close; it hides again after a moment. On tiling compositors a
  drag can be done with the compositor's own binding (e.g. Mod+drag on
  Hyprland) — the toolbar's drag area works where the protocol allows
  it.
- **HW encode toggle** — visible only in builds with the `hw-encode`
  feature and usable only where VAAPI works. It applies the next time
  an encoder is created: stop and re-share to switch.

## Troubleshooting

| symptom | meaning / fix |
|:--|:--|
| `Missing shared libraries` from install.sh | install the packages it names |
| `FINGERPRINT MISMATCH for this server!` | the pin changed. Verify the server's new fingerprint out of band, then delete that line from `~/.config/parley/known_hosts` |
| connect fails immediately | wrong port (remember it is UDP), or the server's firewall |
| join a channel, no sound | check the system mixer; joining opens the default devices |
| share asks nothing / `portal` error | install the ScreenCast backend for your desktop and restart `xdg-desktop-portal` |
| a watched share is black | sender side: the share's `encoder ready` line; receiver side: frames arriving but not decoding means keyframe/stream shape |
| hardware toggle missing | the build lacks `hw-encode`, or VAAPI has no encoder (`vainfo`) |

Logs: run `~/.local/opt/parley/parley` from a terminal to see
`video:`/`voice:`/`window:` lines; the login page shows the build
version.
