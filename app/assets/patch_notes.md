<!-- Parley release notes.

     This file travels inside every build and is what the app shows: on the
     first launch of a version (automatically) and behind Settings → What's
     new. Write it before you tag a release — the bundled copy is the one
     users read, since the GitHub release body is generated from
     CHANGELOG.md and stays on the web.

     Keep it short and user-facing. The app renders it line by line, with no
     markdown engine: `#`/`##` become headings, `-`/`*` become bullets, every
     other line is a paragraph, and **bold** markers and backticks are
     stripped. Emoji and plain Unicode (·, →, —) are fine.
-->

# Five releases, one update.

0.4.4 to 0.4.9 in a single jump. Everything that changed on the way.

## 0.4.4 · Updates from inside the app

- Settings → Updates looks for the newest build, downloads the one for your platform, checks it against the release checksums, installs it and restarts straight into it.
- The handover is boring on purpose: the running install is replaced only after the new one unpacks, and a bad swap puts the old one back. The log is update.log next to the download.
- These notes. They open by themselves on the first launch of a version, and live forever behind What's new.
- A copy you cannot write to (a pacman install in /opt) is refused with a pointer to the package manager instead of being half-updated.

## 0.4.5 · Windows grows up

- Screen sharing on Windows: pick a monitor or a single window.
- Share audio too, either the whole output or only the apps you choose.
- Fullscreen shares really are fullscreen, and the previous window placement comes back on exit.
- An idle desktop stays live instead of freezing on the last frame.

## 0.4.6 · Pokes, wheel, camera

- Poke: click an online participant — in a channel or in the idle list — hit Poke, and they get a notification and a sound. Poke back from the notification.
- Volume without dialogs: hover a user in the channel and scroll the wheel, 5% per notch.
- Webcam sharing on Linux.
- Minor fixes. Porcoddio.

## 0.4.7 · Streams survive motion, camera on Windows

- High motion fix: frames now leave at the stream's bitrate instead of in a burst, the ladder steps down gently and the view-rate floor is 15 fps. (Vediamo se Dario Cucci riesce a streammare più del fotolibro di Rocket League.)
- Windows camera share, BETA. A capitu @AleOcerg? BETAA. L'eventuale combustione spontanea dell'utilizzatore non ha try/catch, ragazzi.
- Camera overlay: your webcam in the corner of any screen share. One stream, one tile, both views.

## 0.4.8 · Profile settings, avatars, quieter failures

- Profile page next to Settings: change username and password. Ta ricordare la vecchia, cuiune.
- Profile picture, picked straight from your files, shown on the roster and next to your chat rows.
- Pokes now ring even with headphones muted, while AFK or deafened. That is the whole point of a poke.
- Server transcoding is opt-in, and a tier that cannot keep up hands the viewer back instead of showing a slideshow.
- Windows camera failsafe: a camera that refuses no longer leaves the share half-started. Microsoft, pijatela nculu plz.

## 0.4.9 · Your sounds, your files

- Personal event sounds: drop join.mp3, leave.mp3 or afk.mp3 in the sounds folder next to the app, then switch them on in Profile → Soundboard. Everyone in the channel hears yours.
- The folder survives updates and reinstalls, 1 MB per file, MP3 only. No file, no problem: the default chime stays.
- Mute everything silences personal sounds. Pokes still get through, because that is the point of a poke.
- Chat file sharing: the paperclip next to the composer sends a file into the channel, up to 100 MB. Click the card to download, the message text becomes the caption. (100 MB di cap — per testare un po', caso mai Fladio Gelo carica l'intramfs come file jpeg.)
- Files live on the server and disappear together with their message in a disappearing channel.

Server first, then clients: files and personal sounds need both sides on 0.4.9. Voice and chat against an older build still work.
