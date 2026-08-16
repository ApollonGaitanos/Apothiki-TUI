# Releasing apothiki

Three separate things, in order. Each one can be checked before the next.

1. **GitHub** holds the source and the tag.
2. **The tag** is what the PKGBUILD downloads and checksums.
3. **The AUR** holds only a `PKGBUILD` and a `.SRCINFO`, in its own git repo.
   That is what makes `paru -S apothiki` work.

Nothing here is reversible in the usual sense — a published tag is one people
may already have built against — so the checksum step is not optional.

## 1. Push the source

```sh
git branch -M main                 # GitHub's default; skip if you prefer master
git remote add origin git@github.com:ApollonG/Apothiki.git
git push -u origin main
```

Create the repository on GitHub first (empty, no README/licence — the tree
already has both). If you use HTTPS rather than SSH, the remote is
`https://github.com/ApollonG/Apothiki.git`.

## 2. Tag the release

```sh
git tag -a v0.1.0 -m "apothiki 0.1.0"
git push origin v0.1.0
```

The tag name matters: the PKGBUILD builds
`$url/archive/refs/tags/v$pkgver.tar.gz`, so `pkgver=0.1.0` requires the tag
`v0.1.0`.

## 3. Fill in the real checksum

The PKGBUILD ships `sha256sums=('SKIP')`, which accepts *whatever* the URL
serves. Shipping that is the exact hazard the program warns about in its own
AUR dialog. Once the tag exists:

```sh
cd packaging
updpkgsums                         # pacman-contrib; downloads the tag, rewrites the line
makepkg -f                         # builds it for real, runs the test suite
```

`makepkg` also proves the tarball's top directory matches — GitHub names it
after the *repository* (`Apothiki-0.1.0`), not the package, and the two differ
in case here.

## 4. Publish to the AUR

This is the "one command" step. It needs an account at
<https://aur.archlinux.org> with your SSH public key added under *My Account*.

```sh
cd packaging
makepkg --printsrcinfo > .SRCINFO

git clone ssh://aur@aur.archlinux.org/apothiki.git /tmp/aur-apothiki
cp PKGBUILD .SRCINFO /tmp/aur-apothiki/
cd /tmp/aur-apothiki
git add PKGBUILD .SRCINFO
git commit -m "apothiki 0.1.0: initial release"
git push
```

The clone of a not-yet-existing package succeeds and is empty; the first push
creates it. `.SRCINFO` must be regenerated and committed on **every** change —
the AUR rejects a push whose `.SRCINFO` disagrees with the `PKGBUILD`.

Then, from anywhere:

```sh
paru -S apothiki
```

## Later releases

```sh
# bump pkgver in packaging/PKGBUILD, reset pkgrel to 1
git commit -am "0.2.0" && git tag -a v0.2.0 -m "apothiki 0.2.0"
git push origin main --tags
cd packaging && updpkgsums && makepkg --printsrcinfo > .SRCINFO
# copy both into the AUR clone, commit, push
```

Bump `pkgrel` instead of `pkgver` when only the packaging changed and the
source tag did not.

## Notes on the PKGBUILD

- `options=(!lto)` is required, not stylistic. makepkg enables LTO by default,
  which puts `-flto=auto` into `CFLAGS`; ring's C and assembly then land in the
  archive as GCC bytecode that lld cannot read, and the link fails on the TLS
  symbols. Cargo already does full LTO on the Rust side.
- `check()` runs the test suite during the build. It touches no system state
  and needs no fixtures, so it is safe in a clean chroot.
- Test the whole thing in a clean chroot before pushing to the AUR, so a
  dependency you happen to have installed does not become an invisible
  requirement:

  ```sh
  # devtools
  extra-x86_64-build
  ```
