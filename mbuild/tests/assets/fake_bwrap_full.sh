#!/usr/bin/env bash
set -euo pipefail

out_host=""
step_name=""
prev=""
bind_src=""
setenv_key=""

for arg in "$@"; do
  case "$prev" in
    bind-src)
      bind_src="$arg"
      prev="bind-dest"
      continue
      ;;
    bind-dest)
      if [ "$arg" = "/__mbuild/out" ]; then
        out_host="$bind_src"
      fi
      prev=""
      continue
      ;;
    setenv-key)
      setenv_key="$arg"
      prev="setenv-value"
      continue
      ;;
    setenv-value)
      if [ "$setenv_key" = "MBUILD_STEP_NAME" ]; then
        step_name="$arg"
      fi
      prev=""
      continue
      ;;
  esac

  case "$arg" in
    --bind)
      prev="bind-src"
      ;;
    --setenv)
      prev="setenv-key"
      ;;
  esac
done

if [ -z "$out_host" ]; then
  echo "missing /__mbuild/out bind" >&2
  exit 2
fi

mkdir -p "$out_host"
printf '%s\n' "$step_name" >> "$out_host/container-steps.txt"
