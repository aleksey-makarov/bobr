#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = run ]; then
  if [ "${MBUILD_TEST_BINARY_PODMAN_FAIL:-}" = "1" ]; then
    echo simulated podman run failure >&2
    exit 42
  fi
  shift 1
  source_input=""
  declare -A in_mounts
  out_host=""
  config_host=""
  config_dir=""
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
        elif [ "$mount" = "/__mbuild_script_config" ]; then
          config_host="$host"
        fi
        shift 2
        ;;
      --env)
        kv="$2"
        case "$kv" in
          MBUILD_SOURCE_INPUT=*) source_input="${kv#*=}" ;;
          MBUILD_SCRIPT_CONFIG_DIR=*) config_dir="${kv#*=}" ;;
        esac
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
  if [ -z "$source_input" ]; then
    for key in "${!in_mounts[@]}"; do
      source_input="$key"
      break
    done
  fi
  mkdir -p "$out_host/copied"
  if [ -n "$source_input" ] && [ -n "${in_mounts[$source_input]+x}" ]; then
    cp -R "${in_mounts[$source_input]}/." "$out_host/copied/"
  fi
  if [ -n "$config_host" ]; then
    mkdir -p "$out_host/script-config"
    cp -R "${config_host}/." "$out_host/script-config/" 2>/dev/null || true
    printf '%s\n' "${config_dir:-}" > "$out_host/script-config-dir.txt"
  fi
  printf '%s\n' "$image_ref" > "$out_host/image-ref.txt"
  exit 0
fi

echo unexpected podman invocation: "$@" >&2
exit 1
