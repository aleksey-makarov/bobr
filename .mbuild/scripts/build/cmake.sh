#!/bin/sh
set -eu

src="${MBUILD_SOURCE_INPUT:?MBUILD_SOURCE_INPUT is required}"
out="${MBUILD_PRIMARY_OUTPUT:?MBUILD_PRIMARY_OUTPUT is required}"

cd "/in/${src}"

if [ ! -f CMakeLists.txt ]; then
  candidates=""
  for d in ./*; do
    if [ -d "$d" ] && [ -f "$d/CMakeLists.txt" ]; then
      candidates="$candidates $d"
    fi
  done

  set -- $candidates
  if [ "$#" -eq 1 ]; then
    cd "$1"
  else
    echo "cmake build-script: CMakeLists.txt not found (or ambiguous) in /in/${src}" >&2
    exit 1
  fi
fi

jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="/out/${out}"
cmake --build build -j"$jobs"
cmake --install build
