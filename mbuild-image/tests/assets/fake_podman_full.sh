#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  target="${3:-}"
  if [[ "$target" == __GENERATED_PREFIX__:* ]]; then
    cat <<JSON
[{"Id":"sha256:generated-image","RepoDigests":["${target}@__GENERATED_DIGEST__"]}]
JSON
  else
    cat <<'JSON'
__BASE_INSPECT_JSON__
JSON
  fi
  exit 0
fi
if [ "${1:-}" = import ]; then
  if [ "${MBUILD_TEST_IMAGE_IMPORT_FAIL:-}" = "1" ]; then
    echo simulated podman import failure >&2
    exit 42
  fi
  echo sha256:imported-image
  exit 0
fi
if [ "${1:-}" = create ]; then
  echo ctr-test
  exit 0
fi
if [ "${1:-}" = cp ]; then
  exit 0
fi
if [ "${1:-}" = commit ]; then
  echo sha256:committed-image
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = run ]; then
  shift 1
  declare -A in_mounts
  out_host=""
  image_ref=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --volume)
        spec="$2"
        if [[ "$spec" =~ ^(.*):(/[^:]+):(.*)$ ]]; then
          host="${BASH_REMATCH[1]}"
          mount="${BASH_REMATCH[2]}"
        else
          echo invalid volume spec: "$spec" >&2
          exit 1
        fi
        if [[ "$mount" == /in/* ]]; then
          name="${mount#/in/}"
          in_mounts["$name"]="$host"
        elif [ "$mount" = "/out/out" ]; then
          out_host="$host"
        fi
        shift 2
        ;;
      --rm|--network=none|--userns=keep-id)
        shift 1
        ;;
      --user)
        shift 2
        ;;
      *)
        if [ -z "$image_ref" ]; then
          image_ref="$1"
        fi
        shift 1
        ;;
    esac
  done
  source_input="sources0"
  if [ -z "${in_mounts[$source_input]+x}" ]; then
    for key in "${!in_mounts[@]}"; do
      source_input="$key"
      break
    done
  fi
  if [ -z "$source_input" ] || [ -z "${in_mounts[$source_input]+x}" ]; then
    echo missing source input mount >&2
    exit 1
  fi
  mkdir -p "$out_host/copied"
  cp -R "${in_mounts[$source_input]}/." "$out_host/copied/"
  printf '%s\n' "$image_ref" > "$out_host/image-ref.txt"
  exit 0
fi

echo unexpected podman invocation: "$@" >&2
exit 1
