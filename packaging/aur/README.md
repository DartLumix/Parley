# AUR packaging

`parley-bin` repackages the Linux bundle. Users install it with `pacman`;
nothing is compiled on their machine.

**Parked.** The AUR was not accepting new account registrations when this
was prepared, so the package has not been pushed. Everything below is the
plan for the day it is unparked; nothing here is needed for in-app updates.

## Where the tarball comes from

This was written while the application repository was private, when AUR
sources had to be fetchable without credentials, and the PKGBUILD still
points `_mirror` at the separate public repository made for that. The
application repository is public now, so `_mirror` and `url` can name the
release page directly (`https://github.com/DartLumix/Parley`) and the
archive comes from the `v<version>` release the workflow already published,
with its `SHA256SUMS`. Repoint those two lines when the package is pushed.

The other decision to settle then is which Linux tarball to repackage:
`tools/package-client.sh` builds one natively on Arch, while the release page
carries the Ubuntu-built artifact that CI resolves against a clean Ubuntu
24.04. Either works for `pacman`; the in-app updater always installs the
release page's copy.

## One-time setup

1. Create an AUR account and add this machine's public key at
   <https://aur.archlinux.org/account>. `ssh -T aur@aur.archlinux.org`
   should greet you (account registration has to be open again).
2. Clone the package:

       git clone ssh://aur@aur.archlinux.org/parley-bin.git /tmp/parley-bin-aur

## Each release

1. Build the bundle on Arch:

       tools/package-client.sh <version> dist

2. Nothing to upload: the release workflow already published the tarball
   (that release is also the in-app update channel — see docs/release.md,
   *In-app updates*). If the tarball is built locally instead, do not
   rebuild between checksumming it and using it: those exact bytes are what
   the checksum covers.

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
