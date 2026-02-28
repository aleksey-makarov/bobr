#!/bin/sh
set -eu

src="${MBUILD_SOURCE_INPUT:?MBUILD_SOURCE_INPUT is required}"
out="${MBUILD_PRIMARY_OUTPUT:?MBUILD_PRIMARY_OUTPUT is required}"

cd "/in/${src}"

if [ ! -x ./configure ]; then
  candidates=""
  for d in ./*; do
    if [ -d "$d" ] && [ -x "$d/configure" ]; then
      candidates="$candidates $d"
    fi
  done

  set -- $candidates
  if [ "$#" -eq 1 ]; then
    cd "$1"
  fi
fi

if [ ! -x ./configure ]; then
  if [ -x ./autogen.sh ]; then ./autogen.sh; fi
  if [ -x ./bootstrap ]; then ./bootstrap; fi
fi

if [ ! -x ./configure ]; then
  echo "autotools build-script: ./configure not found in /in/${src}" >&2
  exit 1
fi

jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

./configure --prefix="/out/${out}"
make -j"$jobs"
make install
