pkgname=rkvm-git
pkgver=0.6.1.r1.gf237ac8
pkgrel=1
pkgdesc='Virtual KVM switch for Linux machines (local build)'
arch=('x86_64')
url='https://github.com/htrefil/rkvm'
license=('MIT')
depends=('libevdev')
makedepends=('git' 'cargo' 'clang')
options=('!lto')
provides=('rkvm')
conflicts=('rkvm')

pkgver() {
  cd "$startdir"
  git describe --long --tags | sed 's/\([^-]*-g\)/r\1/;s/-/./g'
}

build() {
  cd "$startdir"
  cargo build --release --locked
}

package() {
  cd "$startdir"
  install -Dm755 target/release/rkvm-server "$pkgdir/usr/bin/rkvm-server"
  install -Dm755 target/release/rkvm-client "$pkgdir/usr/bin/rkvm-client"
  install -Dm755 target/release/rkvm-certificate-gen "$pkgdir/usr/bin/rkvm-certificate-gen"
  install -Dm644 systemd/rkvm-server.service "$pkgdir/usr/lib/systemd/system/rkvm-server.service"
  install -Dm644 systemd/rkvm-client.service "$pkgdir/usr/lib/systemd/system/rkvm-client.service"
  install -Dm644 example/server.toml "$pkgdir/usr/share/rkvm/examples/server.toml"
  install -Dm644 example/client.toml "$pkgdir/usr/share/rkvm/examples/client.toml"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/rkvm-git/LICENSE"
}
