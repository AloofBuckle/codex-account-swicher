#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
topdir=$(mktemp -d)
staged_rpm=
cleanup() {
    if [[ -n "$staged_rpm" ]]; then
        rm -f -- "$staged_rpm"
    fi
    rm -rf -- "$topdir"
}
trap cleanup EXIT

# Publish to the existing DNF repository after every successful RPM build.
# CAS_PUBLISH_REPO=0           Build the RPM without publishing it.
# CAS_RPM_REPO_DIR=/some/path  Override the default DNF repository directory.
# CAS_ALLOW_RPM_REPLACE=1      Explicitly allow replacing a published RPM with
#                              different contents but the same filename/NEVRA.
publish_repo=${CAS_PUBLISH_REPO:-1}
rpm_repo_dir=${CAS_RPM_REPO_DIR:-/files/archive/cas}
allow_replace=${CAS_ALLOW_RPM_REPLACE:-0}
case "$publish_repo:$allow_replace" in
    0:0|0:1|1:0|1:1) ;;
    *)
        printf 'CAS_PUBLISH_REPO and CAS_ALLOW_RPM_REPLACE must be 0 or 1\n' >&2
        exit 1
        ;;
esac

if [[ "$publish_repo" == 1 ]]; then
    if [[ ! -d "$rpm_repo_dir" || ! -f "$rpm_repo_dir/repodata/repomd.xml" ]]; then
        printf 'Existing DNF repository not found: %s\n' "$rpm_repo_dir" >&2
        printf 'Set CAS_RPM_REPO_DIR or use CAS_PUBLISH_REPO=0 for a local-only build.\n' >&2
        exit 1
    fi
    command -v createrepo_c >/dev/null || {
        printf 'createrepo_c is required to publish a DNF repository\n' >&2
        exit 1
    }
    command -v flock >/dev/null || {
        printf 'flock is required for safe concurrent DNF repository updates\n' >&2
        exit 1
    }
fi

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
mapfile -d '' -t packages < <(find "$topdir/RPMS" -type f -name 'cas-*.rpm' -print0)
if [[ "${#packages[@]}" -ne 1 ]]; then
    printf 'Expected exactly one built CAS RPM; found %s\n' "${#packages[@]}" >&2
    exit 1
fi
rpm_name=$(basename -- "${packages[0]}")
rpm_output="target/rpm/$rpm_name"
install -m0644 -- "${packages[0]}" "$rpm_output"
rpm -K "$rpm_output"
printf 'RPM output:\n'
printf '  %s\n' "$rpm_output"

if [[ "$publish_repo" == 1 ]]; then
    # Lock the entire copy-and-index operation. Keep old versioned RPMs so
    # clients can roll back using their normal DNF workflow.
    lock_file="$rpm_repo_dir/.cas-publish.lock"
    if [[ -L "$lock_file" ]]; then
        printf 'Refusing to use a symlinked repository lock: %s\n' "$lock_file" >&2
        exit 1
    fi
    # Shared wheel-group repository: a root-owned 0644 lock file would block
    # later builds by another authorized publisher.
    previous_umask=$(umask)
    umask 0002
    exec {repo_lock_fd}>>"$lock_file"
    umask "$previous_umask"
    flock -x "$repo_lock_fd"

    published_rpm="$rpm_repo_dir/$rpm_name"
    if [[ -e "$published_rpm" && ! -f "$published_rpm" ]]; then
        printf 'Refusing to replace a non-regular RPM target: %s\n' "$published_rpm" >&2
        exit 1
    fi
    if [[ -L "$published_rpm" ]]; then
        printf 'Refusing to replace a symlinked RPM target: %s\n' "$published_rpm" >&2
        exit 1
    fi
    if [[ -f "$published_rpm" ]] && ! cmp -s -- "$rpm_output" "$published_rpm"; then
        if [[ "$allow_replace" != 1 ]]; then
            printf 'Different RPM already published under the same filename: %s\n' "$published_rpm" >&2
            printf 'Bump the RPM version/release; or set CAS_ALLOW_RPM_REPLACE=1 to explicitly overwrite.\n' >&2
            exit 1
        fi
        printf 'Warning: replacing an existing RPM with the same version; clients may have cached it.\n' >&2
    fi

    if [[ ! -f "$published_rpm" ]] || ! cmp -s -- "$rpm_output" "$published_rpm"; then
        # Stage the payload on the same filesystem and rename it atomically.
        staged_rpm=$(mktemp "$rpm_repo_dir/.cas-rpm-XXXXXXXX")
        install -m0644 -- "$rpm_output" "$staged_rpm"
        cmp -s -- "$rpm_output" "$staged_rpm" || {
            printf 'Staged RPM contents differ from the build artifact\n' >&2
            exit 1
        }
        mv -f -- "$staged_rpm" "$published_rpm"
        staged_rpm=
    fi

    createrepo_c --update --workers 2 "$rpm_repo_dir"
    if [[ ! -s "$rpm_repo_dir/repodata/repomd.xml" ]] || ! cmp -s -- "$rpm_output" "$published_rpm"; then
        printf 'DNF repository publication verification failed\n' >&2
        exit 1
    fi
    printf 'DNF repository updated:\n  %s\n  %s\n' "$published_rpm" "$rpm_repo_dir/repodata/repomd.xml"
fi
