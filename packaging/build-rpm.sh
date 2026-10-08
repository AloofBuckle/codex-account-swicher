#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
topdir=$(mktemp -d)
trap 'rm -rf "$topdir"' EXIT

cd "$repo_root"
cargo build --release -p cas-cli

mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
install -m0755 target/release/cas "$topdir/SOURCES/cas"
install -m0644 packaging/cas.spec "$topdir/SPECS/cas.spec"

rpmbuild -bb \
    --define "_topdir $topdir" \
    "$topdir/SPECS/cas.spec"

mkdir -p target/rpm
find "$topdir/RPMS" -type f -name 'cas-*.rpm' -exec cp -f {} target/rpm/ \;
printf 'RPM output:\n'
find target/rpm -maxdepth 1 -type f -name 'cas-*.rpm' -print
