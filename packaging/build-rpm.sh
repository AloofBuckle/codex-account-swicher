#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
topdir=$(mktemp -d)
trap 'rm -rf "$topdir"' EXIT

cd "$repo_root"
build_target=${CAS_BUILD_TARGET:-x86_64-unknown-linux-musl}
cargo build --release -p cas-cli --target "$build_target"

mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
install -m0755 "target/$build_target/release/cas" "$topdir/SOURCES/cas"
install -m0644 LICENSE "$topdir/SOURCES/LICENSE"
install -m0644 packaging/cas.spec "$topdir/SPECS/cas.spec"

rpmbuild -bb \
    --define "_topdir $topdir" \
    "$topdir/SPECS/cas.spec"

mkdir -p target/rpm
find "$topdir/RPMS" -type f -name 'cas-*.rpm' -exec cp -f {} target/rpm/ \;
printf 'RPM output:\n'
find target/rpm -maxdepth 1 -type f -name 'cas-*.rpm' -print
