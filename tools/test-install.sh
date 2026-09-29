#!/usr/bin/env bash

# Exercise install.sh against a local release fixture and a restricted PATH.

set -euo pipefail

die() {
  echo "test-install.sh: $*" >&2
  exit 1
}

script_path="$(readlink -f "${BASH_SOURCE[0]}")"
repo="$(cd "$(dirname "${script_path}")/.." && pwd)"
installer="${repo}/install.sh"
test_root="$(mktemp -d)"
cleanup() {
  rm -rf -- "${test_root}"
}
trap cleanup EXIT

version=9.8.7
target=x86_64-unknown-linux-musl
archive_name="bobr-${target}.tar.xz"
archive_root="bobr-v${version}-${target}"
fixture="${test_root}/release"
fixture_root="${test_root}/fixture/${archive_root}"
mkdir -p "${fixture}" "${fixture_root}/bin"

cat >"${fixture_root}/bin/bobr" <<'EOF'
#!/bin/sh
case "${1:-}" in
  --version)
    echo 'bobr 9.8.7 (request bobr-request-v6) (0123456789abcdef0123456789abcdef01234567)'
    ;;
  --build-info)
    if [ -n "${BOBR_TEST_BAD_BUILD_INFO:-}" ]; then
      echo '{"version":"9.8.7","request_schema":"bobr-request-v6","provenance":{"git_commit":"0123456789abcdef0123456789abcdef01234567","git_dirty":true}}'
    else
      echo '{"version":"9.8.7","request_schema":"bobr-request-v6","provenance":{"git_commit":"0123456789abcdef0123456789abcdef01234567","git_dirty":false}}'
    fi
    ;;
  *) exit 2 ;;
esac
EOF
cat >"${fixture_root}/bin/bobr-repo" <<'EOF'
#!/bin/sh
[ "${1:-}" = --version ] || exit 2
echo 'bobr-repo 9.8.7'
EOF
cat >"${fixture_root}/bin/bobr-fsobj-hash" <<'EOF'
#!/bin/sh
[ "${1:-}" = --help ] || exit 2
echo 'fixture hash help'
EOF
cat >"${fixture_root}/bin/bobr-sandbox-launcher" <<'EOF'
#!/bin/sh
[ "${1:-}" = --protocol-info ] || exit 2
echo '{"name":"bobr-sandbox-launcher","protocol_version":6}'
EOF
chmod 755 "${fixture_root}/bin/"*
tar -C "${test_root}/fixture" -cJf "${fixture}/${archive_name}" \
  "${archive_root}"
archive_checksum="$(sha256sum "${fixture}/${archive_name}")"
archive_checksum="${archive_checksum%% *}"
printf '%s  %s\n' "${archive_checksum}" "${archive_name}" \
  >"${fixture}/SHA256SUMS"
printf '%064d  %s\n' 0 "${archive_name}" \
  >"${fixture}/SHA256SUMS.bad"

tool_bin="${test_root}/tools"
mkdir "${tool_bin}"
for tool in awk bash cp mkdir mktemp mv rm sha256sum tar xz; do
  tool_path="$(command -v "${tool}")"
  [ -n "${tool_path}" ] || die "test prerequisite is missing: ${tool}"
  ln -s "${tool_path}" "${tool_bin}/${tool}"
done
real_uname="$(command -v uname)"
cat >"${tool_bin}/uname" <<EOF
#!/usr/bin/env bash
set -euo pipefail
case "\${1:-}" in
  -s)
    if [ -n "\${BOBR_TEST_UNAME_S:-}" ]; then
      printf '%s\\n' "\${BOBR_TEST_UNAME_S}"
      exit 0
    fi
    ;;
  -m)
    if [ -n "\${BOBR_TEST_UNAME_M:-}" ]; then
      printf '%s\\n' "\${BOBR_TEST_UNAME_M}"
      exit 0
    fi
    ;;
esac
exec ${real_uname@Q} "\$@"
EOF
chmod 755 "${tool_bin}/uname"
real_install="$(command -v install)"
cat >"${tool_bin}/install" <<EOF
#!/usr/bin/env bash
set -euo pipefail
destination="\${!#}"
if [ -n "\${BOBR_TEST_INSTALL_FAILURE:-}" ]; then
  case "\${destination}" in
    */"\${BOBR_TEST_INSTALL_FAILURE}") exit 70 ;;
  esac
fi
exec ${real_install@Q} "\$@"
EOF
chmod 755 "${tool_bin}/install"
cat >"${tool_bin}/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --proto | --output)
      [ "$#" -ge 2 ] || exit 64
      if [ "$1" = --output ]; then
        output="$2"
      fi
      shift 2
      ;;
    --tlsv1.2 | --fail | --location | --silent | --show-error)
      shift
      ;;
    http://* | https://*)
      url="$1"
      shift
      ;;
    *) exit 64 ;;
  esac
done
[ -n "${output}" ] && [ -n "${url}" ] || exit 64
prefix='https://github.com/aleksey-makarov/bobr/releases/latest/download/'
case "${url}" in
  "${prefix}"*) name="${url#${prefix}}" ;;
  *) exit 65 ;;
esac
if [ "${BOBR_TEST_DOWNLOAD_FAILURE:-}" = "${name}" ]; then
  exit 22
fi
if [ "${name}" = SHA256SUMS ] && [ -n "${BOBR_TEST_BAD_CHECKSUM:-}" ]; then
  cp "${BOBR_TEST_FIXTURE}/SHA256SUMS.bad" "${output}"
else
  cp "${BOBR_TEST_FIXTURE}/${name}" "${output}"
fi
EOF
chmod 755 "${tool_bin}/curl"

env_command="$(command -v env)"
run_installer() {
  "${env_command}" -i \
    HOME="$1" \
    PATH="${tool_bin}" \
    BOBR_TEST_FIXTURE="${fixture}" \
    "${tool_bin}/bash" "${installer}" "${@:2}"
}

commands=(bobr bobr-repo bobr-fsobj-hash bobr-sandbox-launcher)
home="${test_root}/home"
mkdir -p "${home}/.local/bin"
for command in "${commands[@]}"; do
  printf 'old-%s\n' "${command}" >"${home}/.local/bin/${command}"
done
printf '%s\n' legacy-fetch >"${home}/.local/bin/bobr-fetch"
printf '%s\n' legacy-hash >"${home}/.local/bin/fsobj-hash"
run_installer "${home}" >"${test_root}/success.stdout" \
  2>"${test_root}/success.stderr"
for command in "${commands[@]}"; do
  [ -x "${home}/.local/bin/${command}" ] \
    || die "successful install omitted ${command}"
  [ "$(cat "${home}/.local/bin/${command}")" != "old-${command}" ] \
    || die "successful install did not replace ${command}"
done
[ "$(cat "${home}/.local/bin/bobr-fetch")" = legacy-fetch ] \
  || die "installer changed legacy bobr-fetch"
[ "$(cat "${home}/.local/bin/fsobj-hash")" = legacy-hash ] \
  || die "installer changed legacy fsobj-hash"

env_bin="${test_root}/env-bin"
"${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_INSTALL_DIR="${env_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  "${tool_bin}/bash" "${installer}" \
  >"${test_root}/environment.stdout" 2>"${test_root}/environment.stderr"
[ -x "${env_bin}/bobr" ] || die "BOBR_INSTALL_DIR was not used"

precedence_env_bin="${test_root}/precedence-env-bin"
flag_bin="${test_root}/flag-bin"
"${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_INSTALL_DIR="${precedence_env_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  "${tool_bin}/bash" "${installer}" --bin-dir "${flag_bin}" \
  >"${test_root}/precedence.stdout" 2>"${test_root}/precedence.stderr"
[ -x "${flag_bin}/bobr" ] || die "--bin-dir was not used"
[ ! -e "${precedence_env_bin}" ] \
  || die "BOBR_INSTALL_DIR overrode --bin-dir"

if "${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  BOBR_TEST_UNAME_M=riscv64 \
  "${tool_bin}/bash" "${installer}" \
  >"${test_root}/platform.stdout" 2>"${test_root}/platform.stderr"; then
  die "installer accepted an unsupported architecture"
fi
grep_command="$(command -v grep)"
"${grep_command}" -Fq 'unsupported architecture: riscv64' \
  "${test_root}/platform.stderr" \
  || die "unsupported architecture error is not precise"

missing_tool_bin="${test_root}/missing-tool-bin"
mkdir "${missing_tool_bin}"
for tool in awk curl install mkdir mktemp mv rm sha256sum tar uname; do
  ln -s "${tool_bin}/${tool}" "${missing_tool_bin}/${tool}"
done
if "${env_command}" -i \
  HOME="${home}" \
  PATH="${missing_tool_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  "${tool_bin}/bash" "${installer}" \
  >"${test_root}/missing-tool.stdout" 2>"${test_root}/missing-tool.stderr"; then
  die "installer succeeded without xz"
fi
"${grep_command}" -Fq 'xz not found on PATH' \
  "${test_root}/missing-tool.stderr" \
  || die "missing-tool error is not precise"

failure_bin="${test_root}/failure-bin"
mkdir "${failure_bin}"
for command in "${commands[@]}"; do
  printf 'old-%s\n' "${command}" >"${failure_bin}/${command}"
done
assert_old_install() {
  local command
  for command in "${commands[@]}"; do
    [ "$(cat "${failure_bin}/${command}")" = "old-${command}" ] \
      || die "failed install changed ${command}"
  done
}

if "${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  BOBR_TEST_BAD_CHECKSUM=1 \
  "${tool_bin}/bash" "${installer}" --bin-dir "${failure_bin}" \
  >"${test_root}/checksum.stdout" 2>"${test_root}/checksum.stderr"; then
  die "installer accepted an invalid checksum"
fi
assert_old_install

if "${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  BOBR_TEST_BAD_BUILD_INFO=1 \
  "${tool_bin}/bash" "${installer}" --bin-dir "${failure_bin}" \
  >"${test_root}/build-info.stdout" \
  2>"${test_root}/build-info.stderr"; then
  die "installer accepted dirty release provenance"
fi
assert_old_install

if "${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  BOBR_TEST_DOWNLOAD_FAILURE="${archive_name}" \
  "${tool_bin}/bash" "${installer}" --bin-dir "${failure_bin}" \
  >"${test_root}/download.stdout" 2>"${test_root}/download.stderr"; then
  die "installer ignored a failed download"
fi
assert_old_install

if "${env_command}" -i \
  HOME="${home}" \
  PATH="${tool_bin}" \
  BOBR_TEST_FIXTURE="${fixture}" \
  BOBR_TEST_INSTALL_FAILURE=bobr-repo \
  "${tool_bin}/bash" "${installer}" --bin-dir "${failure_bin}" \
  >"${test_root}/install.stdout" 2>"${test_root}/install.stderr"; then
  die "installer ignored a staging failure"
fi
assert_old_install

for path in "${failure_bin}"/.bobr-install.*; do
  [ ! -e "${path}" ] \
    || die "failed install left staging files in the destination"
done

echo "test-install.sh: ok"
