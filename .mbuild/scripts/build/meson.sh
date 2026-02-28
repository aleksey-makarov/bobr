#!/bin/sh
set -eu

src="${MBUILD_SOURCE_INPUT:?MBUILD_SOURCE_INPUT is required}"
out="${MBUILD_PRIMARY_OUTPUT:?MBUILD_PRIMARY_OUTPUT is required}"

cd "/in/${src}"

if [ ! -f meson.build ]; then
  candidates=""
  for d in ./*; do
    if [ -d "$d" ] && [ -f "$d/meson.build" ]; then
      candidates="$candidates $d"
    fi
  done

  set -- $candidates
  if [ "$#" -eq 1 ]; then
    cd "$1"
  else
    echo "meson build-script: meson.build not found (or ambiguous) in /in/${src}" >&2
    exit 1
  fi
fi

jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

meson setup build --prefix="/out/${out}"
meson compile -C build -j"$jobs"
meson install -C build
