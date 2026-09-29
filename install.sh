#!/usr/bin/env bash

# Install the latest Bobr release for this machine.
#
# Usage:
#   install.sh [--bin-dir DIR]
#
#   --bin-dir DIR  install into DIR instead of BOBR_INSTALL_DIR or
#                  $HOME/.local/bin

set -euo pipefail

program="${0##*/}"

die() {
  echo "${program}: $*" >&2
  exit 2
}

usage() {
  printf '%s\n' \
    'Install the latest Bobr release for this machine.' \
    '' \
    'Usage:' \
    '  install.sh [--bin-dir DIR]' \
    '' \
    '  --bin-dir DIR  install into DIR instead of BOBR_INSTALL_DIR or' \
    "                 \$HOME/.local/bin"
}

requested_bin_dir=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --bin-dir)
      [ "$#" -ge 2 ] || die "--bin-dir requires a directory"
      [ -n "$2" ] || die "--bin-dir requires a non-empty directory"
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

for command in awk curl install mkdir mktemp mv rm sha256sum tar uname xz; do
  command -v "${command}" >/dev/null 2>&1 \
    || die "${command} not found on PATH"
done

[ "$(uname -s)" = Linux ] \
  || die "unsupported operating system: $(uname -s) (Linux is required)"
case "$(uname -m)" in
  x86_64)
    target="x86_64-unknown-linux-musl"
    ;;
  aarch64 | arm64)
    die "Linux AArch64 releases are not published yet"
    ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

if [ -z "${requested_bin_dir}" ]; then
  if [ -n "${BOBR_INSTALL_DIR:-}" ]; then
    requested_bin_dir="${BOBR_INSTALL_DIR}"
  else
    [ -n "${HOME:-}" ] \
      || die "HOME is not set; use BOBR_INSTALL_DIR or --bin-dir"
    requested_bin_dir="${HOME}/.local/bin"
  fi
fi

invocation_dir="${PWD}"
case "${requested_bin_dir}" in
  /*) ;;
  *) requested_bin_dir="${invocation_dir}/${requested_bin_dir}" ;;
esac

release_url="https://github.com/aleksey-makarov/bobr/releases/latest/download"
archive_name="bobr-${target}.tar.xz"
work_dir="$(mktemp -d)"
install_stage=
cleanup() {
  rm -rf -- "${work_dir}"
  if [ -n "${install_stage}" ]; then
    rm -rf -- "${install_stage}"
  fi
}
trap cleanup EXIT

download() {
  local name="$1"
  curl \
    --proto '=https' \
    --tlsv1.2 \
    --fail \
    --location \
    --silent \
    --show-error \
    --output "${work_dir}/${name}" \
    "${release_url}/${name}"
}

echo "downloading the latest Bobr release for ${target}" >&2
download "${archive_name}"
download SHA256SUMS

expected_checksum="$(
  awk -v name="${archive_name}" '
    $2 == name {
      count++
      checksum = $1
    }
    END {
      if (count != 1) {
        exit 1
      }
      print checksum
    }
  ' "${work_dir}/SHA256SUMS"
)" || die "SHA256SUMS does not contain exactly one entry for ${archive_name}"
[[ "${expected_checksum}" =~ ^[0-9a-f]{64}$ ]] \
  || die "SHA256SUMS contains an invalid digest for ${archive_name}"
actual_checksum="$(sha256sum "${work_dir}/${archive_name}")"
actual_checksum="${actual_checksum%% *}"
[ "${actual_checksum}" = "${expected_checksum}" ] \
  || die "checksum mismatch for ${archive_name}"

listing="${work_dir}/archive-list"
tar -tJf "${work_dir}/${archive_name}" >"${listing}" \
  || die "failed to list ${archive_name}"
archive_root=
found_root=0
while IFS= read -r entry; do
  [ -n "${entry}" ] || die "archive contains an empty path"
  case "${entry}" in
    /* | ../* | */../* | */..)
      die "archive contains an unsafe path: ${entry}"
      ;;
  esac
  entry_root="${entry%%/*}"
  [ -n "${entry_root}" ] || die "archive contains an invalid path: ${entry}"
  if [ -z "${archive_root}" ]; then
    archive_root="${entry_root}"
  elif [ "${entry_root}" != "${archive_root}" ]; then
    die "archive contains more than one top-level path"
  fi
  if [ "${entry}" = "${archive_root}/" ]; then
    found_root=1
  fi
done <"${listing}"
[ -n "${archive_root}" ] || die "archive is empty"
[ "${found_root}" -eq 1 ] \
  || die "archive does not contain its top-level directory entry"

root_prefix="bobr-v"
root_suffix="-${target}"
case "${archive_root}" in
  "${root_prefix}"*"${root_suffix}") ;;
  *) die "unexpected archive root: ${archive_root}" ;;
esac
version="${archive_root#"${root_prefix}"}"
version="${version%"${root_suffix}"}"
[[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "archive root contains an invalid Bobr version: ${archive_root}"

extract_dir="${work_dir}/extract"
mkdir "${extract_dir}"
tar --no-same-owner -xJf "${work_dir}/${archive_name}" -C "${extract_dir}" \
  || die "failed to extract ${archive_name}"
staged_bin="${extract_dir}/${archive_root}/bin"
commands=(bobr bobr-repo bobr-fsobj-hash bobr-sandbox-launcher)
for command in "${commands[@]}"; do
  [ -f "${staged_bin}/${command}" ] \
    || die "archive is missing bin/${command}"
  [ -x "${staged_bin}/${command}" ] \
    || die "archive contains a non-executable bin/${command}"
done

build_info="$("${staged_bin}/bobr" --build-info)" \
  || die "staged bobr failed to report build information"
build_info_prefix="{\"version\":\"${version}\",\"request_schema\":\"bobr-request-v6\",\"provenance\":{\"git_commit\":\""
build_info_suffix='","git_dirty":false}}'
case "${build_info}" in
  "${build_info_prefix}"*"${build_info_suffix}") ;;
  *) die "staged bobr reports invalid build information" ;;
esac
git_commit="${build_info#"${build_info_prefix}"}"
git_commit="${git_commit%"${build_info_suffix}"}"
[[ "${git_commit}" =~ ^[0-9a-f]{40}$ ]] \
  || die "staged bobr reports an invalid Git commit"
[ "${build_info}" = "${build_info_prefix}${git_commit}${build_info_suffix}" ] \
  || die "staged bobr reports invalid build information"
expected_version="bobr ${version} (request bobr-request-v6) (${git_commit})"
[ "$("${staged_bin}/bobr" --version)" = "${expected_version}" ] \
  || die "staged bobr reports an unexpected version"
[ "$("${staged_bin}/bobr-repo" --version)" = "bobr-repo ${version}" ] \
  || die "staged bobr-repo reports an unexpected version"
"${staged_bin}/bobr-fsobj-hash" --help >/dev/null \
  || die "staged bobr-fsobj-hash failed its smoke check"
launcher_protocol="$("${staged_bin}/bobr-sandbox-launcher" --protocol-info)" \
  || die "staged sandbox launcher failed to report its protocol"
[ "${launcher_protocol}" = \
    '{"name":"bobr-sandbox-launcher","protocol_version":6}' ] \
  || die "staged sandbox launcher reports an unexpected protocol"

# Nothing above this point changes the destination. Stage all four files on
# the destination filesystem before beginning the sequence of atomic renames.
mkdir -p "${requested_bin_dir}"
bin_dir="$(cd "${requested_bin_dir}" && pwd -P)"
install_stage="$(mktemp -d "${bin_dir}/.bobr-install.XXXXXX")"
for command in "${commands[@]}"; do
  install -m755 "${staged_bin}/${command}" "${install_stage}/${command}"
done
for command in "${commands[@]}"; do
  mv -f "${install_stage}/${command}" "${bin_dir}/${command}"
done
rm -rf -- "${install_stage}"
install_stage=

hash -r
selected_bobr="$(command -v bobr 2>/dev/null || true)"
echo "installed bobr ${version} into ${bin_dir}" >&2
if [ -z "${selected_bobr}" ]; then
  echo "add the installation directory to PATH:" >&2
  echo "  export PATH=\"${bin_dir}:\$PATH\"" >&2
elif [ "${selected_bobr}" != "${bin_dir}/bobr" ]; then
  echo "note: PATH currently selects ${selected_bobr}" >&2
  echo "      installed bobr is ${bin_dir}/bobr" >&2
  echo "put the installation directory first:" >&2
  echo "  export PATH=\"${bin_dir}:\$PATH\"" >&2
fi
