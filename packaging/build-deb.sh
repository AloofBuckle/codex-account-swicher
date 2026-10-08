#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

cd "$repo_root"
version=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
build_target=${CAS_BUILD_TARGET:-x86_64-unknown-linux-musl}

case "$build_target" in
    x86_64-*) deb_arch=amd64 ;;
    aarch64-*) deb_arch=arm64 ;;
    *)
        printf 'Unsupported Debian target: %s\n' "$build_target" >&2
        exit 1
        ;;
esac

cargo build --release -p cas-cli --target "$build_target"

pkgroot="$workdir/pkgroot"
controldir="$workdir/control"
mkdir -p "$pkgroot/usr/bin" "$pkgroot/usr/share/doc/cas" "$controldir" target/deb
install -m0755 "target/$build_target/release/cas" "$pkgroot/usr/bin/cas"
install -m0644 LICENSE "$pkgroot/usr/share/doc/cas/copyright"

cat > "$controldir/control" <<EOF
Package: cas
Version: $version
Section: utils
Priority: optional
Architecture: $deb_arch
Maintainer: AloofBuckle <102452965+AloofBuckle@users.noreply.github.com>
Homepage: https://github.com/AloofBuckle/codex-account-swicher
Description: ChatGPT account switcher for Codex
 CAS stores multiple Codex ChatGPT credentials and switches the active
 .codex/auth.json credential.
EOF

printf '2.0\n' > "$workdir/debian-binary"
tar -C "$controldir" --owner=0 --group=0 -cJf "$workdir/control.tar.xz" .
tar -C "$pkgroot" --owner=0 --group=0 -cJf "$workdir/data.tar.xz" .

output="target/deb/cas_${version}_${deb_arch}.deb"
rm -f "$output"
(cd "$workdir" && ar r "$repo_root/$output" debian-binary control.tar.xz data.tar.xz >/dev/null)
printf 'DEB output:\n%s\n' "$output"
