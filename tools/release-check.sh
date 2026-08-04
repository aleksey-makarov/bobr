#!/usr/bin/env bash

# Runs everything the release workflow does, short of publishing, so a tag is a
# formality over an already-checked state rather than the first time any of it
# is exercised.
#
# Usage:
#   tools/release-check.sh [TAG]
#
#   TAG   the tag you are about to create (default: v<workspace version>)
#
# It exists because the release path has parts that nothing else runs: the
# packaging script builds a request by hand, checks the launcher's protocol
# version, and verifies static linkage, and none of that is reached by `cargo
# test` or by CI on master. A schema bump once broke it, and the break surfaced
# only after the tag had been pushed.
#
# What it cannot check: the aarch64 half. The build needs the target installed
# and is skipped without it, and the launcher tests cannot run natively on an
# x86_64 host at all -- those stay with CI.

set -euo pipefail

die() {
  echo "release-check.sh: $*" >&2
  exit 2
}

step() { echo "==> $*" >&2; }

[ "$#" -le 1 ] || die "usage: $(basename "$0") [TAG]"

script_path="$(readlink -f "${BASH_SOURCE[0]}")"
repo="$(cd "$(dirname "${script_path}")/.." && pwd)"
cd "${repo}"

command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH"
command -v strip >/dev/null 2>&1 || die "strip not found on PATH"

# Read the version exactly as the workflow's prepare job does, so that a
# mismatch is caught here rather than by a job that has already started.
version="$(sed -n '/^\[workspace.package\]$/,/^\[/s/^version = "\([^"]*\)"$/\1/p' Cargo.toml)"
[ -n "${version}" ] || die "failed to read the workspace version from Cargo.toml"
expected_tag="v${version}"
tag="${1:-${expected_tag}}"
[ "${tag}" = "${expected_tag}" ] \
  || die "tag ${tag} does not match the workspace version ${expected_tag}"

case "$(uname -m)" in
  x86_64) host_target="x86_64-unknown-linux-musl" ;;
  aarch64) host_target="aarch64-unknown-linux-musl" ;;
  *) die "unsupported host architecture: $(uname -m)" ;;
esac
case "${host_target}" in
  x86_64-*) other_target="aarch64-unknown-linux-musl" ;;
  *) other_target="x86_64-unknown-linux-musl" ;;
esac

# Reproducible archives: the workflow derives this from the tagged commit, so
# use the same source here rather than the current time.
source_date_epoch="$(git -C "${repo}" log -1 --format=%ct)"
out="$(mktemp -d)"
trap 'rm -rf "${out}"' EXIT

step "checks (the workflow's test job)"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

step "main archive, ${host_target}"
cargo build --release --locked --target "${host_target}" \
  -p bobr-build -p fsobj-hash -p bobr-sandbox-launcher
.github/scripts/package-release.sh main "${tag}" "${host_target}" \
  "${source_date_epoch}" "${out}"

step "bundle launcher, ${host_target}"
cargo test --locked -p bobr-bundle-launcher
cargo build --release --locked --target "${host_target}" -p bobr-bundle-launcher
.github/scripts/package-release.sh bundle "${tag}" "${host_target}" \
  "${source_date_epoch}" "${out}"

# The other architecture is build-only even when its target is installed: its
# tests need a machine of that architecture to run on.
if cargo build --release --locked --target "${other_target}" \
  -p bobr-bundle-launcher 2>/dev/null; then
  step "bundle launcher, ${other_target} (build and package only)"
  .github/scripts/package-release.sh bundle "${tag}" "${other_target}" \
    "${source_date_epoch}" "${out}"
else
  echo "note: skipping ${other_target}; install the target to cover it" >&2
fi

step "archives built for ${tag}"
ls -l "${out}" >&2
echo "release-check.sh: ok" >&2
