# Maintainer: Apollon Gaitanos <apollon.gaitanos@gmail.com>
pkgname=apothiki
pkgver=0.1.0
pkgrel=1
pkgdesc="Application-centric package explorer for Arch Linux"
arch=('x86_64')
url="https://github.com/ApollonG/apothiki"
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
  install -Dm0644 APOTHIKI_SPEC.md "$pkgdir/usr/share/doc/$pkgname/APOTHIKI_SPEC.md"
}
