{
  "zstd-bin-smoke-1.5.7" = {
    type = "binary",
    inputs = [ "zstd-1.5.7" ],
    outputs = [ "zstd-bin-smoke-1.5.7" ],
    script = m%"
      #!/bin/sh
      set -eu
      echo "mbbin smoke build for zstd-1.5.7" > /out/zstd-bin-smoke-1.5.7/BUILD.txt
      find /in/zstd-1.5.7 -maxdepth 2 -type f | head -n 50 > /out/zstd-bin-smoke-1.5.7/FILES.txt
    "%,
  },

  "zstd-bin-1.5.7" = {
    type = "binary",
    inputs = [ "zstd-1.5.7" ],
    outputs = [ "zstd-bin-1.5.7" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/zstd-1.5.7

      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
      make -j"$jobs"

      install -d /out/zstd-bin-1.5.7/bin
      install -m 0755 programs/zstd /out/zstd-bin-1.5.7/bin/zstd
      /out/zstd-bin-1.5.7/bin/zstd --version > /out/zstd-bin-1.5.7/VERSION.txt
    "%,
  },
}
