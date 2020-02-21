#!/usr/bin/env bash

find . -type f -name "*.ncl" -print0 | xargs -0 -I{} sh -c 'echo "Formatting: {}"; nickel format "{}" || echo "Failed: {}" >&2'