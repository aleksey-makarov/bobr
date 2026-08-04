#!/usr/bin/env bash

# Build deterministic bobr release archives from already-built Cargo outputs.
# Usage: package-release.sh main|bundle RELEASE_TAG TARGET SOURCE_DATE_EPOCH OUT

set -euo pipefail

die() {
  echo "package-release.sh: $*" >&2
  exit 2
}

[ "$#" -eq 5 ] || die "expected: main|bundle RELEASE_TAG TARGET SOURCE_DATE_EPOCH OUT"

kind="$1"
release_tag="$2"
target="$3"
source_date_epoch="$4"
output_dir="$5"

case "${kind}" in
  main | bundle) ;;
  *) die "unknown package kind '${kind}'" ;;
esac
[[ "${release_tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "invalid release tag '${release_tag}'"
[[ "${source_date_epoch}" =~ ^[0-9]+$ ]] \
  || die "invalid SOURCE_DATE_EPOCH '${source_date_epoch}'"

case "${target}" in
  x86_64-unknown-linux-musl)
    machine_pattern="Advanced Micro Devices X86-64"
    ;;
  aarch64-unknown-linux-musl)
    machine_pattern="AArch64"
    ;;
  *)
    die "unsupported release target '${target}'"
    ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target_dir="${repo_root}/target/${target}/release"
output_dir="$(mkdir -p "${output_dir}" && cd "${output_dir}" && pwd)"
staging="$(mktemp -d)"

cleanup() {
  rm -rf -- "${staging}"
}
trap cleanup EXIT

require_file() {
  local path="$1"
  [ -f "${path}" ] || die "missing built binary '${path}'"
}

verify_static_elf() {
  local path="$1"
  [ -x "${path}" ] || die "release binary is not executable: ${path}"
  readelf -h "${path}" | grep -Eq "Machine:[[:space:]]+${machine_pattern}" \
    || die "release binary has the wrong architecture: ${path}"
  if readelf -l "${path}" | grep -q 'INTERP'; then
    die "release binary contains PT_INTERP: ${path}"
  fi
  if readelf -d "${path}" | grep -q '(NEEDED)'; then
    die "release binary contains DT_NEEDED: ${path}"
  fi
}

make_archive() {
  local root_name="$1"
  local archive="${output_dir}/${root_name}.tar.xz"
  tar \
    --format=gnu \
    --sort=name \
    --mtime="@${source_date_epoch}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "${staging}" \
    -cJf "${archive}" \
    "${root_name}"
  echo "created ${archive}" >&2
}

if [ "${kind}" = "main" ]; then
  [ "${target}" = "x86_64-unknown-linux-musl" ] \
    || die "the main bobr archive is currently x86_64-only"

  root_name="bobr-${release_tag}-${target}"
  root="${staging}/${root_name}"
  mkdir -p "${root}/bin"
  for binary in bobr fsobj-hash bobr-sandbox-launcher; do
    require_file "${target_dir}/${binary}"
    install -m755 "${target_dir}/${binary}" "${root}/bin/${binary}"
    strip "${root}/bin/${binary}"
    verify_static_elf "${root}/bin/${binary}"
  done
  install -m644 "${repo_root}/README.md" "${root}/README.md"
  install -m644 "${repo_root}/LICENSE-APACHE" "${root}/LICENSE-APACHE"
  install -m644 "${repo_root}/LICENSE-MIT" "${root}/LICENSE-MIT"

  "${root}/bin/fsobj-hash" --help >/dev/null
  protocol_info="$("${root}/bin/bobr-sandbox-launcher" --protocol-info)"
  [ "${protocol_info}" = '{"name":"bobr-sandbox-launcher","protocol_version":6}' ] \
    || die "unexpected sandbox launcher protocol info: ${protocol_info}"

  smoke="${staging}/smoke"
  # bobr creates none of these itself, so that a mistyped path fails at once.
  # The run directories share the store's filesystem, which it also checks.
  mkdir -p "${smoke}/store" "${smoke}/store/logs/release-smoke" \
    "${smoke}/store/work/release-smoke"
  cat >"${smoke}/request.json" <<EOF
{
  "schema": "bobr-request-v2",
  "store": "${smoke}/store",
  "logs": "${smoke}/store/logs/release-smoke",
  "work": "${smoke}/store/work/release-smoke",
  "run_id": "release-smoke",
  "nodes": {
    "root": {
      "name": "release-smoke",
      "tag": "Tree",
      "config": {
        "tree": {
          "entries": [
            {
              "type": "file",
              "path": "release-smoke.txt",
              "text": "release smoke test\\n",
              "executable": false
            }
          ]
        }
      },
      "inputs": {}
    }
  }
}
EOF
  object_hash="$("${root}/bin/bobr" "${smoke}/request.json")"
  [[ "${object_hash}" =~ ^[0-9a-f]{64}$ ]] \
    || die "bobr smoke test returned an invalid object hash: ${object_hash}"

  make_archive "${root_name}"
  exit 0
fi

root_name="bobr-bundle-launcher-${release_tag}-${target}"
root="${staging}/${root_name}"
require_file "${target_dir}/bobr-bundle-launcher"
install -Dm755 \
  "${target_dir}/bobr-bundle-launcher" \
  "${root}/usr/libexec/bobr-bundle-launcher"
strip "${root}/usr/libexec/bobr-bundle-launcher"
verify_static_elf "${root}/usr/libexec/bobr-bundle-launcher"

launcher_stdout="${staging}/launcher.stdout"
launcher_stderr="${staging}/launcher.stderr"
if "${root}/usr/libexec/bobr-bundle-launcher" \
  >"${launcher_stdout}" 2>"${launcher_stderr}"; then
  die "bundle launcher without an invocation mode unexpectedly succeeded"
fi
grep -Fq 'usage: bobr-bundle-launcher --run TOOL' "${launcher_stderr}" \
  || die "bundle launcher smoke test did not print the expected usage"

make_archive "${root_name}"
