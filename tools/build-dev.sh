#!/usr/bin/env bash

# Builds this checkout and installs it where the rest of the development
# environment expects to find it.
#
# Usage:
#   tools/build-dev.sh [--quick] [--debug]
#
#   --quick   build and install only, skipping the checks
#   --debug   build the debug profile (default: release)
#
# It installs into <workspace>/bobr-bin/bin (override with BOBR_DEV_BIN), which a
# developer keeps on PATH. Everything downstream -- the recipes' bin/bobr-build.sh,
# the QEMU bundles, bobr-rebuild-world.sh -- then finds the binaries the same way
# a user finds an unpacked release, so there is nothing special about the
# development path beyond which binaries are on PATH.
#
# The layout mirrors the release archive on purpose: `bobr` locates
# `bobr-sandbox-launcher` beside its own executable, so the two have to travel
# together.
#
# This script does not build recipes. Once it has run, build them with the
# recipes' bin/bobr-build.sh as usual.

set -euo pipefail

die() {
  echo "build-dev.sh: $*" >&2
  exit 2
}

quick=0
profile=release
while [ "$#" -gt 0 ]; do
  case "$1" in
    --quick) quick=1; shift ;;
    --debug) profile=debug; shift ;;
    -h | --help) sed -n '3,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 0 ;;
    *) die "unexpected argument: $1" ;;
  esac
done

script_path="$(readlink -f "${BASH_SOURCE[0]}")"
bobr_repo="$(cd "$(dirname "${script_path}")/.." && pwd)"
workspace_root="$(cd "${bobr_repo}/.." && pwd)"
bin_dir="${BOBR_DEV_BIN:-${workspace_root}/bobr-bin/bin}"

command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH"

case "$(uname -m)" in
  x86_64) musl_target="x86_64-unknown-linux-musl" ;;
  aarch64) musl_target="aarch64-unknown-linux-musl" ;;
  *) die "unsupported host architecture: $(uname -m)" ;;
esac
arch_suffix="${musl_target%%-*}"

cargo_profile_args=()
[ "${profile}" = "release" ] && cargo_profile_args+=(--release)

step() { echo "==> $*" >&2; }

cd "${bobr_repo}"

# Formatting first: it takes no time at all, and there is no sense finding a
# stray space after three minutes of compiling.
if [ "${quick}" -eq 0 ]; then
  step "cargo fmt --all --check"
  cargo fmt --all --check
fi

step "cargo build (${profile})"
cargo build "${cargo_profile_args[@]}"

step "cargo build-sandbox-launcher-${arch_suffix} (${profile})"
cargo "build-sandbox-launcher-${arch_suffix}" "${cargo_profile_args[@]}"

# Built to prove it still compiles, not to install: recipes fetch the bundle
# launcher from a published release rather than from this tree.
step "cargo build-bundle-launcher-${arch_suffix} (${profile})"
cargo "build-bundle-launcher-${arch_suffix}" "${cargo_profile_args[@]}"

# The checks run in the default (debug) profile: they compile faster there, and
# nothing about them depends on the profile the installed binaries use.
if [ "${quick}" -eq 0 ]; then
  step "cargo clippy --workspace --all-targets"
  cargo clippy --workspace --all-targets

  step "cargo test --workspace --all-features"
  cargo test --workspace --all-features

  # Also a check: broken intra-doc links are denied workspace-wide.
  step "cargo doc --workspace --no-deps"
  cargo doc --workspace --no-deps
fi

# Install last, so a failed build never replaces working binaries. Each file goes
# in through a temporary name in the same directory, so an interrupted copy
# cannot leave a half-written binary behind.
install_binary() {
  local source="$1" name="$2" temp
  [ -f "${source}" ] || die "cargo did not produce ${source}"
  temp="${bin_dir}/.${name}.new.$$"
  install -m755 "${source}" "${temp}"
  mv -f "${temp}" "${bin_dir}/${name}"
}

mkdir -p "${bin_dir}"
step "install into ${bin_dir}"
install_binary "target/${profile}/bobr" bobr
install_binary "target/${profile}/fsobj-hash" fsobj-hash
install_binary "target/${musl_target}/${profile}/bobr-sandbox-launcher" bobr-sandbox-launcher

"${bin_dir}/bobr" --version >&2

case ":${PATH}:" in
  *":${bin_dir}:"*) ;;
  *) echo "note: ${bin_dir} is not on PATH" >&2 ;;
esac
