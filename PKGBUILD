# Maintainer: Apollon Gaitanos <apollon.gaitanos@gmail.com>
pkgname=apothiki
pkgver=0.1.0
pkgrel=1
pkgdesc="Application-centric package explorer for Arch Linux"
arch=('x86_64')
# TODO: point this at the real repository before publishing. The source= line
# below cannot resolve until this exists.
url="https://github.com/ApollonG/apothiki"
# TODO: confirm. Nothing in the tree states a licence yet, and this line is a
# placeholder rather than a decision — add a LICENSE file to match whatever you
# choose, or change both together.
license=('MIT')
# pacman is a runtime dependency in the literal sense: every mutation shells out
# to it, and the read-only oracles the tool checks itself against are its output.
depends=('pacman')
makedepends=('cargo')
# All optional. Each degrades to a clearly-stated absence rather than an error:
# no snapper means no snapshot offer, no helper means AUR installs are declined
# with a reason, no flatpak means no Flatpak apps in the catalog.
optdepends=(
  'snapper: pre-transaction snapshots before removals'
  'paru: installing and updating AUR packages'
  'yay: alternative AUR helper'
  'flatpak: managing Flatpak applications'
  'archlinux-appstream-data: richer descriptions and icons for uninstalled packages'
)
source=("$pkgname-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
# SKIP is only acceptable while the tag does not exist. Replace with the real
# checksum before this is published anywhere: an unverified source in a PKGBUILD
# is exactly the hazard this program warns users about in its own AUR dialog.
sha256sums=('SKIP')

prepare() {
  cd "$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release --all-features
}

check() {
  cd "$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  # The suite touches no system state and needs no fixtures, so it is safe to
  # run in a clean chroot.
  cargo test --frozen --release
}

package() {
  cd "$pkgname-$pkgver"
  install -Dm0755 "target/release/apo" "$pkgdir/usr/bin/apo"
  install -Dm0644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  # Ships only if one exists; see the licence note above.
  [ -f LICENSE ] && install -Dm0644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm0644 APOTHIKI_SPEC.md "$pkgdir/usr/share/doc/$pkgname/APOTHIKI_SPEC.md"
}
