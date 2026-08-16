#!/usr/bin/env bash
# Publishes packaging/PKGBUILD to the AUR.
#
# The AUR is a git remote that holds two files: a PKGBUILD and the .SRCINFO
# generated from it. It rejects a push whose .SRCINFO disagrees with its
# PKGBUILD, so the two are regenerated together here rather than trusted to
# stay in step by hand.
#
# Requires an account at https://aur.archlinux.org/register with your SSH
# public key added afterwards at https://aur.archlinux.org/account/<user>/edit.
# Note that https://aur.archlinux.org/account/ on its own 404s unless you are
# logged in, which reads like the site is broken when it is not. The check
# below is up front on purpose: finding out about a missing key after a
# half-finished clone is worse than not starting.
set -euo pipefail

# Which package to publish. Defaults to the release PKGBUILD beside this
# script; pass a directory for the others, e.g. ./publish-aur.sh apothiki-git.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${1:-$here}"
[ -f PKGBUILD ] || { echo "error: no PKGBUILD in $(pwd)" >&2; exit 1; }
pkgname="$(makepkg --printsrcinfo | sed -n 's/^pkgbase = //p')"
echo "==> publishing $pkgname from $(pwd)"

if ! ssh -o BatchMode=yes -o ConnectTimeout=15 aur@aur.archlinux.org help >/dev/null 2>&1; then
  cat >&2 <<EOF
error: the AUR refused this SSH key.

Register at https://aur.archlinux.org/register if you have not already,
then add the key below at https://aur.archlinux.org/account/<user>/edit
under "SSH Public Key", and run this again:

$(cat ~/.ssh/id_ed25519.pub 2>/dev/null || echo "  (no ~/.ssh/id_ed25519.pub found)")
EOF
  exit 1
fi

# A release PKGBUILD must checksum its tarball, or every user who installs it
# gets a source that cannot be verified. A VCS package is the exception: a git
# source is pinned by the ref it is cloned from and has no fixed tarball.
if grep -q "sha256sums=('SKIP')" PKGBUILD && ! grep -q '^source=.*git+' PKGBUILD; then
  echo "error: PKGBUILD still has sha256sums=('SKIP'); run updpkgsums first" >&2
  exit 1
fi

echo "==> regenerating .SRCINFO"
makepkg --printsrcinfo > .SRCINFO

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "==> cloning the AUR repository (empty is normal for a new package)"
git clone -q "ssh://aur@aur.archlinux.org/${pkgname}.git" "$work/$pkgname"

cp PKGBUILD .SRCINFO "$work/$pkgname/"
cd "$work/$pkgname"

if git diff --quiet --exit-code 2>/dev/null && [ -z "$(git status --porcelain)" ]; then
  echo "==> the AUR already has exactly this; nothing to push"
  exit 0
fi

pkgver="$(sed -n 's/^\tpkgver = //p' .SRCINFO)"
pkgrel="$(sed -n 's/^\tpkgrel = //p' .SRCINFO)"

git add PKGBUILD .SRCINFO
git -c user.name="$(git -C "$here" config user.name)" \
    -c user.email="$(git -C "$here" config user.email)" \
    commit -q -m "${pkgname} ${pkgver}-${pkgrel}"

echo "==> pushing"
git push -q origin master

echo
echo "Published. Anyone on Arch can now install it with:"
echo
echo "    paru -S ${pkgname}"
echo
