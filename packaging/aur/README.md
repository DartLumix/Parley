# AUR packaging

`parley-bin` repackages the **Arch-built** Linux bundle. Users install it
with `pacman`; nothing is compiled on their machine.

## Why a mirror

The application repository is private, and AUR sources must be fetchable
without credentials. The PKGBUILD therefore points `_mirror` at a public
release. If the files move, change that one line (and `url`).

Use the tarball produced by `tools/package-client.sh`, not the Linux
artifact from the release workflow: that one is built on Ubuntu and links
Ubuntu sonames, which do not exist on Arch.

## One-time setup

1. Create the public mirror repository named in `_mirror`
   (`DartLumix/parley-releases` by default). On GitHub: **New
   repository** → Public → create; then **Releases** → **Draft a new
   release** → tag `v0.4.0` → attach
   `dist/parley-0.4.0-linux-x86_64.tar.gz` → publish. The repository
   must be public: a private one returns 404 to every AUR user.
2. Create an AUR account and add this machine's public key at
   <https://aur.archlinux.org/account>. `ssh -T aur@aur.archlinux.org`
   should greet you.
3. Clone the package:

       git clone ssh://aur@aur.archlinux.org/parley-bin.git /tmp/parley-bin-aur

## Each release

1. Build the bundle on Arch:

       tools/package-client.sh <version> dist

2. Upload `dist/parley-<version>-linux-x86_64.tar.gz` to a `v<version>`
   release on the mirror. Do not rebuild between checksumming and
   uploading: those exact bytes are what the checksum covers.

3. Set `pkgver` (and reset `pkgrel=1`), then refresh the checksum and
   metadata:

       cd packaging/aur/parley-bin
       SRCDEST=$(git rev-parse --show-toplevel)/dist updpkgsums
       makepkg --printsrcinfo > .SRCINFO

4. Build and check the package. The tarball is in `dist/`, so this does
   not download it:

       SRCDEST=$(git rev-parse --show-toplevel)/dist makepkg -f
       namcap PKGBUILD parley-bin-*.pkg.tar.zst

5. Copy `PKGBUILD` and `.SRCINFO` into the AUR clone and push:

       cp PKGBUILD .SRCINFO /tmp/parley-bin-aur/
       cd /tmp/parley-bin-aur
       git add PKGBUILD .SRCINFO
       git commit -m "Update to <version>"
       git push

## Notes

- `namcap` reports `ELF files outside of a valid path ('opt/')`: expected.
  The self-contained bundle lives in `/opt/parley`, as the upstream
  installer lays it out.
- If a release is rebuilt without changing its version, bump `pkgrel` and
  refresh the checksum; otherwise pacman sees the same version with
  different bytes.
- `x86_64` only: no other Linux artifact is produced.
