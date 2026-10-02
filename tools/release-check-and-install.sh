#!/usr/bin/env bash

# Run the complete local release gate and install the verified main archive.
#
# Usage:
#   tools/release-check-and-install.sh [--allow-dirty] [--bin-dir DIR]
#
#   --allow-dirty  permit modified or untracked files and record that fact in
#                  the installed binary's build provenance
#   --bin-dir DIR  install into DIR instead of the Cargo-style default
#
# The default destination is $CARGO_INSTALL_ROOT/bin, then $CARGO_HOME/bin,
# then $HOME/.cargo/bin. The release tag used for archive layout is derived
# from the workspace version; this command is intended to run before that tag
# exists.

set -euo pipefail

program="$(basename "$0")"

die() {
  echo "${program}: $*" >&2
  exit 2
}

step() { echo "==> $*" >&2; }

usage() {
  sed -n '3,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

allow_dirty=0
requested_bin_dir=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --allow-dirty)
      allow_dirty=1
      shift
      ;;
    --bin-dir)
      [ "$#" -ge 2 ] || die "--bin-dir requires a directory"
      requested_bin_dir="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unexpected argument: $1" ;;
  esac
done

invocation_dir="${PWD}"
script_path="$(readlink -f "${BASH_SOURCE[0]}")"
repo="$(cd "$(dirname "${script_path}")/.." && pwd)"
cd "${repo}"

for command in awk cargo git grep install readelf sha256sum strip tar; do
  command -v "${command}" >/dev/null 2>&1 \
    || die "${command} not found on PATH"
done
rustc_command="${RUSTC:-rustc}"
command -v "${rustc_command}" >/dev/null 2>&1 \
  || die "${rustc_command} not found on PATH"

read_workflow_toolchain() {
  local workflow="$1"
  local -a values=()

  mapfile -t values < <(
    sed -n \
      "s/^[[:space:]]*RUST_TOOLCHAIN:[[:space:]]*['\"]\([^'\"]*\)['\"][[:space:]]*$/\1/p" \
      "${workflow}"
  )
  [ "${#values[@]}" -eq 1 ] \
    || die "expected exactly one quoted RUST_TOOLCHAIN in ${workflow}"
  printf '%s\n' "${values[0]}"
}

release_workflow="${repo}/.github/workflows/release.yml"
ci_workflow="${repo}/.github/workflows/ci.yml"
release_toolchain="$(read_workflow_toolchain "${release_workflow}")"
ci_toolchain="$(read_workflow_toolchain "${ci_workflow}")"
[ "${release_toolchain}" = "${ci_toolchain}" ] \
  || die "release and CI workflows select different Rust toolchains (${release_toolchain} and ${ci_toolchain})"
local_toolchain="$(
  "${rustc_command}" --version --verbose | sed -n 's/^release: //p'
)"
[ -n "${local_toolchain}" ] \
  || die "failed to read the local rustc release"
[ "${local_toolchain}" = "${release_toolchain}" ] \
  || die "local rustc ${local_toolchain} does not match workflow toolchain ${release_toolchain}"

git_commit="$(git rev-parse --verify HEAD)"
[ -n "${git_commit}" ] || die "failed to resolve the checkout's HEAD commit"
git_status="$(git status --porcelain=v1 --untracked-files=normal)"
if [ -n "${git_status}" ]; then
  [ "${allow_dirty}" -eq 1 ] \
    || die "checkout is dirty; commit the changes or pass --allow-dirty"
  git_dirty=true
else
  git_dirty=false
fi
export BOBR_BUILD_GIT_COMMIT="${git_commit}"
export BOBR_BUILD_GIT_DIRTY="${git_dirty}"

version="$(
  sed -n \
    '/^\[workspace.package\]$/,/^\[/s/^version = "\([^"]*\)"$/\1/p' \
    Cargo.toml
)"
[ -n "${version}" ] \
  || die "failed to read the workspace version from Cargo.toml"
tag="v${version}"

case "$(uname -m)" in
  x86_64)
    host_target="x86_64-unknown-linux-musl"
    ;;
  aarch64)
    die "the main release archive is currently published only for x86_64"
    ;;
  *) die "unsupported host architecture: $(uname -m)" ;;
esac

# Native dependencies must be compiled against musl rather than accidentally
# picking up the host glibc headers and symbols.
command -v musl-gcc >/dev/null 2>&1 \
  || die "musl-gcc not found on PATH (install a native musl C toolchain)"
host_cc_variable="CC_${host_target//-/_}"
export "${host_cc_variable}=musl-gcc"

main_packages=(bobr-build bobr-repo fsobj-hash bobr-sandbox-launcher)
workflow_step="$(
  awk '
    /^ +- name: Build static x86_64 release$/ { in_step = 1; next }
    in_step && /^ +- name: / { exit }
    in_step { print }
  ' "${release_workflow}"
)"
[ -n "${workflow_step}" ] \
  || die "cannot find the 'Build static x86_64 release' step in ${release_workflow}"
workflow_packages="$(
  grep -oE -- '-p +[A-Za-z0-9_-]+' <<<"${workflow_step}" \
    | awk '{ print $2 }' \
    | sort -u
)"
expected_packages="$(printf '%s\n' "${main_packages[@]}" | sort -u)"
if [ "${workflow_packages}" != "${expected_packages}" ]; then
  die "$(printf 'the release workflow builds a different package set than this script.\n  workflow: %s\n  here: %s' \
    "$(tr '\n' ' ' <<<"${workflow_packages}")" \
    "$(tr '\n' ' ' <<<"${expected_packages}")")"
fi

source_date_epoch="$(git show -s --format=%ct HEAD)"
out="$(mktemp -d)"
install_stage=
cleanup() {
  rm -rf -- "${out}"
  if [ -n "${install_stage}" ]; then
    rm -rf -- "${install_stage}"
  fi
}
trap cleanup EXIT

verify_archive() {
  local archive_name="$1"
  local root_name="$2"
  local archive="${out}/${archive_name}"

  [ -f "${archive}" ] || die "missing release archive: ${archive_name}"
  if tar -tJf "${archive}" | awk -F/ -v expected="${root_name}" '
    $1 != expected { invalid = 1 }
    $0 == expected "/" { found_root = 1 }
    END { exit invalid || !found_root }
  '; then
    :
  else
    die "${archive_name} does not contain exactly the root ${root_name}/"
  fi
}

step "checks (the workflow's test job)"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

step "documentation"
cargo doc --workspace --no-deps --locked

step "user installer"
tools/test-install.sh

step "main archive, ${host_target}"
package_flags=()
for package in "${main_packages[@]}"; do
  package_flags+=(-p "${package}")
done
cargo build --release --locked --target "${host_target}" \
  "${package_flags[@]}" --bins
.github/scripts/package-release.sh \
  "${tag}" "${host_target}" "${source_date_epoch}" "${out}"
main_archive="bobr-${host_target}.tar.xz"
main_root="bobr-${tag}-${host_target}"
verify_archive "${main_archive}" "${main_root}"

step "archives built for ${tag}"
find "${out}" -maxdepth 1 -type f -name '*.tar.xz' -printf '%f\n' \
  | LC_ALL=C sort >"${out}/archive-names"
expected_archive_names="${main_archive}"
[ "$(cat "${out}/archive-names")" = "${expected_archive_names}" ] \
  || die "release packaging produced an unexpected archive set"
(
  cd "${out}"
  sha256sum ./*.tar.xz | LC_ALL=C sort -k2 >SHA256SUMS
  sha256sum --check SHA256SUMS
)
ls -l "${out}" >&2

if [ -z "${requested_bin_dir}" ]; then
  if [ -n "${CARGO_INSTALL_ROOT:-}" ]; then
    requested_bin_dir="${CARGO_INSTALL_ROOT}/bin"
  elif [ -n "${CARGO_HOME:-}" ]; then
    requested_bin_dir="${CARGO_HOME}/bin"
  else
    [ -n "${HOME:-}" ] || die "HOME is not set; pass --bin-dir explicitly"
    requested_bin_dir="${HOME}/.cargo/bin"
  fi
fi
case "${requested_bin_dir}" in
  /*) ;;
  *) requested_bin_dir="${invocation_dir}/${requested_bin_dir}" ;;
esac

step "install verified binaries into ${requested_bin_dir}"
mkdir -p "${requested_bin_dir}"
bin_dir="$(cd "${requested_bin_dir}" && pwd -P)"
install_stage="$(mktemp -d "${bin_dir}/.bobr-install.XXXXXX")"
extract_dir="${out}/install"
mkdir "${extract_dir}"
tar -xJf "${out}/${main_archive}" -C "${extract_dir}"

installed_commands=(bobr bobr-repo bobr-fsobj-hash bobr-sandbox-launcher)
for command in "${installed_commands[@]}"; do
  source_path="${extract_dir}/${main_root}/bin/${command}"
  [ -f "${source_path}" ] || die "main archive is missing bin/${command}"
  install -m755 "${source_path}" "${install_stage}/${command}"
done

expected_build_info="$(printf \
  '{"version":"%s","request_schema":"bobr-request-v6","provenance":{"git_commit":"%s","git_dirty":%s}}' \
  "${version}" "${git_commit}" "${git_dirty}")"
staged_build_info="$("${install_stage}/bobr" --build-info)"
[ "${staged_build_info}" = "${expected_build_info}" ] \
  || die "staged bobr reports unexpected build information"
"${install_stage}/bobr-repo" --version >/dev/null
"${install_stage}/bobr-fsobj-hash" --help >/dev/null
[ "$("${install_stage}/bobr-sandbox-launcher" --protocol-info)" = \
    '{"name":"bobr-sandbox-launcher","protocol_version":6}' ] \
  || die "staged sandbox launcher reports an unexpected protocol"

for command in "${installed_commands[@]}"; do
  mv -f "${install_stage}/${command}" "${bin_dir}/${command}"
done
rm -rf -- "${install_stage}"
install_stage=

installed_build_info="$("${bin_dir}/bobr" --build-info)"
[ "${installed_build_info}" = "${expected_build_info}" ] \
  || die "installed bobr reports unexpected build information"

hash -r
selected_bobr="$(command -v bobr 2>/dev/null || true)"
if [ -z "${selected_bobr}" ]; then
  echo "note: bobr is not currently found through PATH; add ${bin_dir}" >&2
elif [ "$(readlink -f "${selected_bobr}")" != "${bin_dir}/bobr" ]; then
  echo "note: PATH selects ${selected_bobr}, not ${bin_dir}/bobr" >&2
fi

echo "installed bobr ${version} into ${bin_dir}" >&2
echo "${installed_build_info}"
