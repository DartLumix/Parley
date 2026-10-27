# Cutting a release

A release is a git tag. Nothing builds downloadable artifacts from
`main` — only `v*` tags do, which is the "stable release" flag.

| tag | meaning |
|:--|:--|
| `v0.2.0` | stable; also tagged `latest` in the registry |
| `v0.2.1-rc1` | prerelease; never `latest` |

## Before tagging

1. Bump the version in `Cargo.toml` (`workspace.package`),
   `app/rust/Cargo.toml` and `app/pubspec.yaml`.
2. Move `CHANGELOG.md`'s unreleased notes under the new version — the
   release body is extracted from that section.
3. From a clean tree:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd app && flutter analyze && flutter test)
```

4. Optionally reproduce the artifacts locally:

```sh
docker build -t parley:local .
cargo build --release -p parley-client && tools/e2e-container.sh
tools/package-client.sh 0.2.0 dist   # needs Flutter on PATH
```

## Tag and push

```sh
git tag -a v0.2.0 -m "Parley 0.2.0"
git push origin v0.2.0
```

## What the workflow does

`.github/workflows/release.yml`, in order:

1. **meta** — derives the version from the tag and the prerelease flag.
2. **verify** — fmt, clippy and the full test suite on the tagged tree.
3. **server** — builds the musl image, extracts the static binary and
   the image tarball, pushes to GHCR (best effort; the tarball is what
   the docs use).
4. **client** — builds the bridge and the Flutter bundle in release
   mode with `--dart-define=PARLEY_VERSION`, packages the client, the
   CLI and the deploy kit (with the server image inside it), then checks
   the bundle's shared libraries on a clean Ubuntu 24.04 (and that the
   system WebRTC dependency is gone).
5. **windows** — builds the bridge and the Flutter bundle on
   `windows-latest` with `audio-device,aec-rust,video` (echo cancellation
   through the pure-Rust AEC3 port; no capture), and zips the bundle. The archive layout is checked by extracting it: a
   `parley.exe` without `parley_flutter.dll` next to it would start and
   then fail on the first bridge call.
6. **smoke** — extracts the kit, loads its image, boots it, adds a user through
   `/parley-server adduser`, logs in as two accounts and proves a wrong
   password is refused (`tools/e2e-container.sh`).
7. **publish** — writes `SHA256SUMS`, extracts the changelog section
   and creates the GitHub release with every asset attached.

Only after all seven jobs pass does anything become public. Re-running a
tag (workflow dispatch) uploads assets with `--clobber` and updates the
notes.

## Assets

| asset | use |
|:--|:--|
| `parley-<v>-linux-x86_64.tar.gz` | desktop client; `./install.sh` |
| `parley-<v>-windows-x86_64.zip` | Windows desktop client; extract, run `parley.exe` |
| `parley-cli-<v>-linux-x86_64.tar.gz` | terminal client, `--check` smoke test |
| `parley-server-<v>-x86_64-linux-musl` | static server without Docker |
| `parley-deploy-<v>.tar.gz` | deploy kit: compose file, env template, runbook **and** `parley-image-<v>.tar.gz` |
| `SHA256SUMS` | `sha256sum -c SHA256SUMS` after downloading |

The image is never a separate asset — installing the kit without it
would only mean downloading the image from somewhere else and loading
it by hand.

## In-app updates

Settings → *Updates* in the desktop client reads `releases/latest` from this
repository and installs the asset named `parley-<v>-linux-x86_64.tar.gz`
(Linux) or `parley-<v>-windows-x86_64.zip` (Windows), after checking it
against the release's `SHA256SUMS`. All three come out of the publish job, so
**a tagged release needs no extra step** for the updater to see it. Override
the channel for a fork or a test build with
`--dart-define=PARLEY_UPDATE_REPO=owner/repo`.

What the updater depends on, and what to check after publishing:

- **Both assets and `SHA256SUMS`** must be attached, and the checksums must
  cover the exact uploaded bytes. The app verifies the download against them
  and installs nothing when they disagree.
- `/releases/latest` **skips prereleases and drafts**: an `-rc` tag is never
  offered, and the last stable release stays the one clients see.
- A release with **no asset for the platform** is reported in the Settings
  card ("release X has no …") rather than failing silently.
- The updater ships **inside the client**, so the first release carrying it
  has to be installed by hand; from then on the app can move forward on its
  own. It never installs an older build.
- A copy the app cannot write to — `/opt/parley` for a pacman install — is
  refused with a pointer to the package manager instead of being modified.

## AUR

`parley-bin` is prepared in [../packaging/aur/README.md](../packaging/aur/README.md).
It will be published separately from this workflow once the AUR account
and public mirror exist; it repackages the Arch-built client tarball,
because AUR cannot fetch from the private repository. Build, checksum
and push steps are in that README.

## Verifying a published release

```sh
sha256sum -c SHA256SUMS
tar xf parley-deploy-0.2.0.tar.gz
docker load -i parley-deploy-0.2.0/parley-image-0.2.0.tar.gz
docker run -d --name parley -p 4443:4443/udp -e PARLEY_ADMIN_PASSWORD=secret parley:0.2.0
cargo build --release -p parley-client
tools/e2e-container.sh          # builds its own image and boots it on a free port
```

Or against the running server, with the shipped CLI:

```sh
./parley-cli --server 127.0.0.1 --user admin --password secret --check
```

The Windows zip is unsigned, so SmartScreen warns on first run
("More info" → "Run anyway"). It carries voice with echo cancellation
but cannot start a screen share — both stated in the release notes
rather than left for the user to discover.
