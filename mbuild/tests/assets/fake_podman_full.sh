#!/usr/bin/env bash
set -euo pipefail

state_root="$(dirname "$0")/.fake-podman-state"
mkdir -p "$state_root"

if [ "${1:-}" = image ] && [ "${2:-}" = exists ]; then
  exit 0
fi

if [ "${1:-}" = --storage-opt ]; then
  shift 2
fi

if [ "${1:-}" = load ]; then
  exit 0
fi

if [ "${1:-}" = create ]; then
  shift 1
  source_input=""
  source_dir=""
  build_dir=""
  install_dir=""
  config_dir=""
  config_host=""
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
          mkdir -p "$state_root/create-mounts"
          printf '%s\n' "$host" > "$state_root/create-mounts/$name"
        elif [ "$mount" = "/__mbuild_script_config" ]; then
          config_host="$host"
        fi
        shift 2
        ;;
      --env)
        kv="$2"
        case "$kv" in
          MBUILD_SOURCE_DIR=*) source_dir="${kv#*=}" ;;
          MBUILD_BUILD_DIR=*) build_dir="${kv#*=}" ;;
          MBUILD_INSTALL_DIR=*) install_dir="${kv#*=}" ;;
          MBUILD_SCRIPT_CONFIG_DIR=*) config_dir="${kv#*=}" ;;
        esac
        shift 2
        ;;
      --network=none|--userns=keep-id)
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

  counter_file="$state_root/counter"
  if [ -f "$counter_file" ]; then
    counter="$(cat "$counter_file")"
  else
    counter=0
  fi
  counter=$((counter + 1))
  printf '%s\n' "$counter" > "$counter_file"
  container_id="fake-container-$counter"
  container_dir="$state_root/$container_id"
  mkdir -p "$container_dir/in-mounts"
  mkdir -p "$container_dir/fs$build_dir"
  mkdir -p "$container_dir/fs$install_dir"
  if [ -d "$state_root/create-mounts" ]; then
    cp -R "$state_root/create-mounts/." "$container_dir/in-mounts/"
    rm -rf "$state_root/create-mounts"
  fi
  printf '%s\n' "$source_input" > "$container_dir/source_input"
  printf '%s\n' "$source_dir" > "$container_dir/source_dir"
  printf '%s\n' "$build_dir" > "$container_dir/build_dir"
  printf '%s\n' "$install_dir" > "$container_dir/install_dir"
  printf '%s\n' "$config_dir" > "$container_dir/config_dir"
  printf '%s\n' "$config_host" > "$container_dir/config_host"
  printf '%s\n' "$image_ref" > "$container_dir/image_ref"
  printf '%s\n' "$container_id"
  exit 0
fi

if [ "${1:-}" = start ]; then
  container_id="$2"
  touch "$state_root/$container_id/started"
  printf '%s\n' "$container_id"
  exit 0
fi

if [ "${1:-}" = exec ]; then
  shift 1
  phase=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --user)
        shift 2
        ;;
      --env)
        kv="$2"
        case "$kv" in
          MBUILD_PHASE=*) phase="${kv#*=}" ;;
        esac
        shift 2
        ;;
      *)
        break
        ;;
    esac
  done

  container_id="$1"
  shift 1
  container_dir="$state_root/$container_id"
  if [ ! -d "$container_dir" ]; then
    echo missing container state for "$container_id" >&2
    exit 1
  fi

  install_dir="$(cat "$container_dir/install_dir")"
  image_ref="$(cat "$container_dir/image_ref")"
  config_host="$(cat "$container_dir/config_host")"
  config_dir="$(cat "$container_dir/config_dir")"
  out_root="$container_dir/fs$install_dir"
  if [ -z "$phase" ]; then
    exit 0
  fi

  case "$phase" in
    configure)
      touch "$container_dir/configured"
      ;;
    build)
      test -f "$container_dir/configured"
      touch "$container_dir/built"
      ;;
    install)
      test -f "$container_dir/built"
      source_input="sources0"
      if [ -f "$container_dir/in-mounts/$source_input" ]; then
        mkdir -p "$out_root/copied"
        cp -R "$(cat "$container_dir/in-mounts/$source_input")/." "$out_root/copied/"
      fi
      printf '%s\n' "$image_ref" > "$out_root/image-ref.txt"
      ;;
    post_install)
      if [ -n "$config_host" ]; then
        mkdir -p "$out_root/script-config"
        cp -R "$config_host/." "$out_root/script-config/" 2>/dev/null || true
        printf '%s\n' "$config_dir" > "$out_root/script-config-dir.txt"
      fi
      ;;
    *)
      echo unsupported phase: "$phase" >&2
      exit 1
      ;;
  esac
  exit 0
fi

if [ "${1:-}" = cp ]; then
  shift 1
  src="$1"
  dest="$2"
  if [[ ! "$src" =~ ^([^:]+):(.+)$ ]]; then
    echo invalid podman cp source: "$src" >&2
    exit 1
  fi
  container_id="${BASH_REMATCH[1]}"
  container_path="${BASH_REMATCH[2]}"
  container_dir="$state_root/$container_id"
  src_path="$container_dir/fs${container_path%/.}"
  mkdir -p "$dest"
  cp -R "$src_path/." "$dest/"
  exit 0
fi

if [ "${1:-}" = rm ]; then
  shift 1
  if [ "${1:-}" = --force ]; then
    shift 1
  fi
  container_id="$1"
  rm -rf "$state_root/$container_id"
  exit 0
fi

echo unexpected podman invocation: "$@" >&2
exit 1
