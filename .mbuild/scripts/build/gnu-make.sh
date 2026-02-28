#!/bin/sh
set -eu

src="${MBUILD_SOURCE_INPUT:?MBUILD_SOURCE_INPUT is required}"
out="${MBUILD_PRIMARY_OUTPUT:?MBUILD_PRIMARY_OUTPUT is required}"

cd "/in/${src}"
jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

make -j"$jobs"

install -d "/out/${out}/bin"

for candidate in ./programs/* ./bin/* ./src/*; do
  if [ -f "$candidate" ] && [ -x "$candidate" ]; then
    install -m 0755 "$candidate" "/out/${out}/bin/$(basename "$candidate")"
  fi
done

if [ -z "$(find "/out/${out}" -mindepth 1 -print -quit 2>/dev/null || true)" ]; then
  echo "gnu-make build produced no outputs in /out/${out}" >&2
  exit 1
fi
