#!/usr/bin/env bash

# Build the deterministic bobr release archive from already-built Cargo outputs.
# Usage: package-release.sh RELEASE_TAG TARGET SOURCE_DATE_EPOCH OUT

set -euo pipefail

die() {
  echo "package-release.sh: $*" >&2
  exit 2
}

[ "$#" -eq 4 ] || die "expected: RELEASE_TAG TARGET SOURCE_DATE_EPOCH OUT"

release_tag="$1"
target="$2"
source_date_epoch="$3"
output_dir="$4"

[[ "${release_tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "invalid release tag '${release_tag}'"
[[ "${source_date_epoch}" =~ ^[0-9]+$ ]] \
  || die "invalid SOURCE_DATE_EPOCH '${source_date_epoch}'"

[ "${target}" = "x86_64-unknown-linux-musl" ] \
  || die "unsupported release target '${target}'"
machine_pattern="Advanced Micro Devices X86-64"

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
  local archive_name="$2"
  local archive="${output_dir}/${archive_name}"
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

[ -n "${BOBR_BUILD_GIT_COMMIT:-}" ] \
  || die "BOBR_BUILD_GIT_COMMIT is required for the main release archive"
case "${BOBR_BUILD_GIT_DIRTY:-}" in
  false | true) ;;
  *) die "BOBR_BUILD_GIT_DIRTY must be 'true' or 'false' for the main release archive" ;;
esac

root_name="bobr-${release_tag}-${target}"
root="${staging}/${root_name}"
mkdir -p "${root}/bin"
for binary in bobr bobr-repo bobr-fsobj-hash bobr-sandbox-launcher; do
  require_file "${target_dir}/${binary}"
  install -m755 "${target_dir}/${binary}" "${root}/bin/${binary}"
  strip "${root}/bin/${binary}"
  verify_static_elf "${root}/bin/${binary}"
done
install -m644 "${repo_root}/README.md" "${root}/README.md"
install -m644 "${repo_root}/LICENSE-APACHE" "${root}/LICENSE-APACHE"
install -m644 "${repo_root}/LICENSE-MIT" "${root}/LICENSE-MIT"

"${root}/bin/bobr-fsobj-hash" --help >/dev/null
"${root}/bin/bobr-repo" --version >/dev/null
bobr_version="$("${root}/bin/bobr" --version)"
expected_provenance="${BOBR_BUILD_GIT_COMMIT}"
if [ "${BOBR_BUILD_GIT_DIRTY}" = true ]; then
  expected_provenance="${expected_provenance}-dirty"
fi
expected_bobr_version="$(printf \
  'bobr %s (request bobr-request-v6) (%s)' \
  "${release_tag#v}" "${expected_provenance}")"
[ "${bobr_version}" = "${expected_bobr_version}" ] \
  || die "unexpected bobr version output: ${bobr_version}"
build_info="$("${root}/bin/bobr" --build-info)"
expected_build_info="$(printf \
  '{"version":"%s","request_schema":"bobr-request-v6","provenance":{"git_commit":"%s","git_dirty":%s}}' \
  "${release_tag#v}" "${BOBR_BUILD_GIT_COMMIT}" "${BOBR_BUILD_GIT_DIRTY}")"
[ "${build_info}" = "${expected_build_info}" ] \
  || die "unexpected bobr build information: ${build_info}"
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
  "schema": "bobr-request-v6",
  "store": "${smoke}/store",
  "logs": "${smoke}/store/logs/release-smoke",
  "work": "${smoke}/store/work/release-smoke",
  "run_id": "release-smoke",
  "quiet": true,
  "goals": ["root"],
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

make_archive "${root_name}" "bobr-${target}.tar.xz"
