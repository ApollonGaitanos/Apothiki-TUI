# Releasing apothiki

Two packages live here:

- `PKGBUILD` — **apothiki**, built from a release tag and checksummed. Needs
  the tag-and-checksum dance below.
- `apothiki-git/PKGBUILD` — **apothiki-git**, built from `main`. Nothing to
  tag and no checksum to refresh, because a git source is pinned by the ref it
  is cloned from. Publish it once and `paru -Sua` picks up every commit after
  that; the trade is that users get whatever is on `main`, tested or not.

They conflict on purpose: both install `/usr/bin/apo`, and pacman should say so
rather than let one silently overwrite the other.

Publish either with `./publish-aur.sh [dir]` — no argument for the release,
`./publish-aur.sh apothiki-git` for the VCS package.

The rest of this file covers the release package: three separate things, in
order, each checkable before the next.

1. **GitHub** holds the source and the tag.
2. **The tag** is what the PKGBUILD downloads and checksums.
3. **The AUR** holds only a `PKGBUILD` and a `.SRCINFO`, in its own git repo.
   That is what makes `paru -S apothiki` work.

Nothing here is reversible in the usual sense — a published tag is one people
may already have built against — so the checksum step is not optional.

## 1. Push the source

```sh
git branch -M main                 # GitHub's default; skip if you prefer master
git remote add origin git@github.com:ApollonGaitanos/Apothiki-TUI.git
git push -u origin main
```

Create the repository on GitHub first (empty, no README/licence — the tree
already has both). If you use HTTPS rather than SSH, the remote is
`https://github.com/ApollonGaitanos/Apothiki-TUI.git`.

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
after the *repository* (`Apothiki-TUI-0.1.0`), not the package (`apothiki`).

## 4. Publish to the AUR

This is the "one command" step. It needs an AUR account, which is separate
from GitHub:

1. Register at <https://aur.archlinux.org/register>.
2. Log in, then add your SSH public key at
   `https://aur.archlinux.org/account/<your-user>/edit` under *SSH Public Key*.
   The bare `/account/` URL 404s when logged out, which looks like an outage
   and is not one.

The AUR authenticates by key alone and refuses the push otherwise. The site is
behind a proof-of-work bot check, so a 503 on the register page is usually
transient — it needs JavaScript enabled and can fail under a strict content
blocker or a VPN exit that is being rate-limited.

```sh
cd packaging && ./publish-aur.sh
```

The script checks the key first, regenerates `.SRCINFO`, refuses to publish a
PKGBUILD still carrying `sha256sums=('SKIP')`, then clones, commits and pushes.
Cloning a package that does not exist yet succeeds and gives an empty
repository; the first push creates it.

Then, from anywhere:

```sh
paru -S apothiki
```

## Planned: apothiki-bin, installing without compiling

Neither package here avoids a build. `apothiki` and `apothiki-git` both compile
from source on the user's machine, which for this crate is roughly seven
minutes — `resvg`, `ring`, `image` and `ratatui` are not small, and the release
profile uses full LTO with `codegen-units = 1`. A `-bin` package downloads an
already-compiled binary from a GitHub release instead and installs in seconds,
the way `visual-studio-code-bin` does.

Three pieces are needed, and none exist yet.

### 1. A release asset

A GitHub release for the tag, carrying a tarball of the compiled binary. Either
built by CI on tag push, or uploaded by hand.

### 2. The binary must not be built the way this machine builds

**This is the part that will bite.** The dev machine's makepkg configuration
sets:

```
RUSTFLAGS="-C opt-level=3 -C target-cpu=native"
CFLAGS="-march=native ..."
```

`native` means "this exact CPU". Counting VEX-encoded (AVX) instructions in
three builds of the same commit on this Coffee Lake i7:

| build | AVX-encoded instructions |
|---|---|
| `-C target-cpu=x86-64` (baseline) | 11,568 |
| `-C target-cpu=native` | 126,646 |
| the installed `apothiki 0.1.0-1` package | 132,432 |

```sh
objdump -d <binary> | grep -cE '\sv[a-z]'
```

An eleven-fold difference. The instruction *mnemonics* are identical across all
three — LLVM is not reaching for exotic operations, it is emitting the AVX
encoding of ordinary ones — which is why a spot check for a single opcode
proves nothing either way, and why the count is the measure to use. The
baseline's remaining 11,568 are runtime-dispatched paths inside dependencies,
guarded by CPUID checks and safe anywhere.

Shipping the native build to someone whose CPU lacks AVX gets them `SIGILL:
illegal instruction` at startup, with no useful message.

That is harmless for `apothiki` and `apothiki-git`, where every user compiles
on their own machine and `native` is exactly right — it is why CachyOS sets it.
It is fatal for anything distributed. A release binary has to pin the baseline
explicitly:

```sh
RUSTFLAGS="-C target-cpu=x86-64" cargo build --release
```

or be built on a clean CI runner, which defaults to the baseline. Building it
here without overriding the environment would produce a package that works
perfectly on this machine and crashes for a stranger — the worst failure shape
there is, because nothing local can reveal it.

### 3. The PKGBUILD

Roughly this, once a release asset exists. Deliberately not committed as a live
file: it would reference a URL that 404s, and a PKGBUILD that looks finished
and cannot build is how the first three packaging bugs in this project happened.

```sh
pkgname=apothiki-bin
pkgver=0.1.0
pkgrel=1
pkgdesc="Application-centric package explorer for Arch Linux (prebuilt)"
arch=('x86_64')
url="https://github.com/ApollonGaitanos/Apothiki-TUI"
license=('MIT')
depends=('pacman' 'glibc' 'gcc-libs')
provides=("apothiki=$pkgver")
conflicts=('apothiki' 'apothiki-git')
source=("$pkgname-$pkgver.tar.gz::$url/releases/download/v$pkgver/apothiki-$pkgver-x86_64.tar.gz")
sha256sums=('...')   # updpkgsums, once the asset is published

package() {
  install -Dm0755 apo "$pkgdir/usr/bin/apo"
  install -Dm0644 LICENSE "$pkgdir/usr/share/licenses/apothiki/LICENSE"
  install -Dm0644 README.md "$pkgdir/usr/share/doc/apothiki/README.md"
}
```

No `makedepends`, no `build()`, no `check()` — there is nothing to compile, and
nothing to test that was not already tested when the binary was produced. Which
is the other reason CI is the right place to build it: the test suite runs
once, on the artefact that ships, rather than on each user's machine.


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
