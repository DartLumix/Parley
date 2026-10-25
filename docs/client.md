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

**Windows.** Download `parley-<version>-windows-x86_64.zip`, extract it
anywhere and run `parley.exe` — nothing to install. Voice with echo
cancellation, chat and watching other people's shares work; starting a
share of your own does not yet (the Share button is absent, not broken),
and the zip is unsigned, so SmartScreen warns on the first run. Settings
and the remembered login live in `%APPDATA%\parley`.

## Connect

    Server:   host or IP, port optional — the default is 4443, so
              "your-server" is enough. The deploy kit publishes 4443
              by default; "your-server:443" only if you changed
              PARLEY_PORT.
    Username: created by the server owner — from Server management in
              their app, or `docker compose exec parley /parley-server
              adduser alice 'password'` for scripted setup
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
- **Layout** — channels and their rosters fill the left pane (a wide
  column, 420 px at the default window size), screens and chat sit on the
  right. A screen share opens above the chat and pushes it down.
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
- **Mute all (headset)** — the slashed-headset button next to it silences
  both directions in one click: your mic goes muted (announced, like the
  mic button) and everything you hear — voices, share audio and the
  join/leave cues — goes silent at the mixer. Unmuting restores the exact
  per-user volumes you had; nothing is replayed from the backlog.
- **AFK** — once your mic is muted, an amber *AFK* button appears beside
  it. It asks for a note ("Coming back later…" is prefilled) that shows
  next to your name in the channel roster, then takes the mic button's
  place. Pressing it again — *Back* — unmutes, makes the output audible
  again and clears the note. Leaving the channel leaves AFK behind.
- **Speaking** — a name lights up in its channel while that person's audio
  is non-silent (measured from the decoded stream, ~10 Hz), and the mic
  button lights with your own capture level. Quiet people stay dark; a
  turned-down person still lights up, because the meter is taken before
  your volume is applied.
- **Volume** — click a name in a channel's roster for their voice slider
  (0–200%). A screen sharer also gets a *Screen share audio* slider on
  their open tile: that stream is trimmed separately, so their voice
  keeps its level.
- **Screen share** — *Share screen* in the floating bar asks for a
  resolution and frame-rate cap first (defaults from Settings, *Source*
  means the capture's own size and rate — caps never upscale), then the
  desktop's portal asks which monitor or window. The same dialog has a
  *Share audio* switch and the list of applications playing right now:
  tick one or more and each is captured from its own PipeWire node, so
  Parley's playback (the voice channel, other shares) is never in the
  stream and a viewer cannot hear themselves back. No guard, no
  subtraction — the echo is excluded by construction. The software encoder
  (openh264) is capped at 1080p whatever *Source* says: measured at
  1440p it needs ~35 ms per frame against a 33 ms slot, so half the
  frames would be dropped. With *Hardware encoding* on, the cap is
  gone and source 1440p60/1080p60 is reachable. The tile's resolution
  line is the stream viewers get, not the capture.
- **Screens** — everyone sharing in your channel (you included) is a 16:9
  tile in the section above the chat: name, resolution, measured frame
  rate, and a *Watch* button. A remote share is a poster until *Watch* is
  pressed — the stream is decoded and measured behind it (so the tile's
  numbers are real and opening is instant) but nothing is converted,
  uploaded or painted until somebody is looking. One share fills the
  section; two split it left/right; three put two on top and centre the
  third below; four make a cross, and so on. *Watch* makes that share big
  and leaves the others as posters in a strip along the bottom — click one
  to open it too. Several can be open: they split the big area, each keeps
  its own system-audio slider, and the close button mutes only that share.
  The big tile's expand button opens real window fullscreen.
- **Your own stream** — while sharing, your tile is fed by the capture tap
  and drawn at the stream's own size and shape (a 21:9 screen stays 21:9;
  the canvas follows the encoder up to 2560×1440), so it shows exactly what
  is being sent rather than a 360p thumbnail of it (a remote share's poster
  does not apply to it). The switch on the tile turns it off, and off stops
  the scaling work rather than just hiding the picture.
  It is an ordinary tile: open it to watch yourself big, expand it for
  the fullscreen page, or leave it small and keep an eye on the others. Its rate line is measured from the frames
  actually sent, so a 60 fps request on a machine that encodes 47 says 47.
- **Settings** — the gear in the app bar: video (hardware encoding,
  default share resolution and frame rate), audio (echo cancellation,
  which applies to the next channel you join), and account (the connected
  server and account, plus *Sign out*). Every control saves immediately
  to `~/.config/parley/settings.toml`; a preference that only takes effect
  at encoder/AEC creation says so on the page.
- **Fullscreen** — the expand button on an open share, your own
  included (yours draws the capture preview, and turning the preview
  switch back on first if it was off). It is real window fullscreen: the
  compositor's own shortcuts work, Esc exits. Opening it never
  interrupts the stream — it is the same open share, drawn on the
  fullscreen page — and only *Stop watching* closes somebody else's.
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
| *Source* on a 1440p/4K display streams 1080p | the software encoder's ceiling (see *Screen share*); enable *Hardware encoding* to send the full size |
| a share stutters while the sender's CPU is busy | software 1080p30 is near the limit on eight cores (~19 ms per frame, p95 ~35 ms). Lower the share height/frame rate, or use hardware encoding |
| a viewer hears their own voice in the shared stream | the sharer is on the whole-output path (`--share-audio` on the CLI), whose monitor includes our playback; the guard subtracts this client's own playback, but a **second** Parley instance playing to the same sink defeats it. In the app, open *Share screen* → *Share audio* and tick the apps to send: each is captured from its own PipeWire node, and Parley's playback is never offered |
| a share stutters while the **viewer's** CPU is busy | the viewer reports its decode rate every 5 s. A current server re-encodes a 720p/480p tier for that viewer alone; otherwise (or at the server's cap) the sharer steps its resolution down one rung at a time and only lowers the frame rate at the 480p floor. The `video: viewer sustaining … fps` line in the viewer's log names the number. If it stays low at a small share, the machine has no decode headroom left |
| own preview stays black | frames are only produced once capture delivers its first buffer — check the `share: encoder ready` line |
| a share's tile says "connecting…" | the first decoded frame has not reached the display yet; if it stays there, check the receiver's `video:` lines |

Logs: run `~/.local/opt/parley/parley` from a terminal to see
`video:`/`voice:`/`window:` lines; the login page shows the build
version.
