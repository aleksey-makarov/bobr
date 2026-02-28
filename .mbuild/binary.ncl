{

  "zstd-1.5.7" = {
    type = "binary",
    inputs = [ "zstd-src-1.5.7" ],
    outputs = [ "zstd-1.5.7" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/zstd-src-1.5.7

      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
      make -j"$jobs"

      install -d /out/zstd-1.5.7/bin
      install -m 0755 programs/zstd /out/zstd-1.5.7/bin/zstd
      /out/zstd-1.5.7/bin/zstd --version > /out/zstd-1.5.7/VERSION.txt
    "%,
  },

  "bc-7.0.3" = {
    type = "binary",
    inputs = [ "bc-src-7.0.3" ],
    outputs = [ "bc-7.0.3" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/bc-src-7.0.3
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/bc-7.0.3
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/bc-7.0.3
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/bc-7.0.3
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/bc-7.0.3/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/bc-7.0.3/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/bc-7.0.3/bin/ \;
        fi
        if [ -n "$(find /out/bc-7.0.3 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for bc-7.0.3" >&2
      exit 1
    "%,
  },

  "expat-2.7.1" = {
    type = "binary",
    inputs = [ "expat-src-2.7.1" ],
    outputs = [ "expat-2.7.1" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/expat-src-2.7.1
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/expat-2.7.1
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/expat-2.7.1
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/expat-2.7.1
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/expat-2.7.1/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/expat-2.7.1/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/expat-2.7.1/bin/ \;
        fi
        if [ -n "$(find /out/expat-2.7.1 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for expat-2.7.1" >&2
      exit 1
    "%,
  },

  "flex-2.6.4" = {
    type = "binary",
    inputs = [ "flex-src-2.6.4" ],
    outputs = [ "flex-2.6.4" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/flex-src-2.6.4
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/flex-2.6.4
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/flex-2.6.4
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/flex-2.6.4
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/flex-2.6.4/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/flex-2.6.4/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/flex-2.6.4/bin/ \;
        fi
        if [ -n "$(find /out/flex-2.6.4 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for flex-2.6.4" >&2
      exit 1
    "%,
  },

  "iana-etc-20250807" = {
    type = "binary",
    inputs = [ "iana-etc-src-20250807" ],
    outputs = [ "iana-etc-20250807" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/iana-etc-src-20250807
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/iana-etc-20250807
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/iana-etc-20250807
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/iana-etc-20250807
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/iana-etc-20250807/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/iana-etc-20250807/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/iana-etc-20250807/bin/ \;
        fi
        if [ -n "$(find /out/iana-etc-20250807 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for iana-etc-20250807" >&2
      exit 1
    "%,
  },

  "libffi-3.5.2" = {
    type = "binary",
    inputs = [ "libffi-src-3.5.2" ],
    outputs = [ "libffi-3.5.2" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/libffi-src-3.5.2
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/libffi-3.5.2
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/libffi-3.5.2
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/libffi-3.5.2
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/libffi-3.5.2/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/libffi-3.5.2/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/libffi-3.5.2/bin/ \;
        fi
        if [ -n "$(find /out/libffi-3.5.2 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for libffi-3.5.2" >&2
      exit 1
    "%,
  },

  "libxcrypt-4.4.38" = {
    type = "binary",
    inputs = [ "libxcrypt-src-4.4.38" ],
    outputs = [ "libxcrypt-4.4.38" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/libxcrypt-src-4.4.38
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/libxcrypt-4.4.38
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/libxcrypt-4.4.38
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/libxcrypt-4.4.38
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/libxcrypt-4.4.38/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/libxcrypt-4.4.38/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/libxcrypt-4.4.38/bin/ \;
        fi
        if [ -n "$(find /out/libxcrypt-4.4.38 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for libxcrypt-4.4.38" >&2
      exit 1
    "%,
  },

  "lz4-1.10.0" = {
    type = "binary",
    inputs = [ "lz4-src-1.10.0" ],
    outputs = [ "lz4-1.10.0" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/lz4-src-1.10.0
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/lz4-1.10.0
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/lz4-1.10.0
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/lz4-1.10.0
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/lz4-1.10.0/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/lz4-1.10.0/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/lz4-1.10.0/bin/ \;
        fi
        if [ -n "$(find /out/lz4-1.10.0 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for lz4-1.10.0" >&2
      exit 1
    "%,
  },

  "meson-1.8.3" = {
    type = "binary",
    inputs = [ "meson-src-1.8.3" ],
    outputs = [ "meson-1.8.3" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/meson-src-1.8.3
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/meson-1.8.3
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/meson-1.8.3
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/meson-1.8.3
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/meson-1.8.3/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/meson-1.8.3/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/meson-1.8.3/bin/ \;
        fi
        if [ -n "$(find /out/meson-1.8.3 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for meson-1.8.3" >&2
      exit 1
    "%,
  },

  "ninja-1.13.1" = {
    type = "binary",
    inputs = [ "ninja-src-1.13.1" ],
    outputs = [ "ninja-1.13.1" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/ninja-src-1.13.1
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/ninja-1.13.1
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/ninja-1.13.1
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/ninja-1.13.1
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/ninja-1.13.1/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/ninja-1.13.1/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/ninja-1.13.1/bin/ \;
        fi
        if [ -n "$(find /out/ninja-1.13.1 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for ninja-1.13.1" >&2
      exit 1
    "%,
  },

  "openssl-3.5.2" = {
    type = "binary",
    inputs = [ "openssl-src-3.5.2" ],
    outputs = [ "openssl-3.5.2" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/openssl-src-3.5.2
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/openssl-3.5.2
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/openssl-3.5.2
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/openssl-3.5.2
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/openssl-3.5.2/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/openssl-3.5.2/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/openssl-3.5.2/bin/ \;
        fi
        if [ -n "$(find /out/openssl-3.5.2 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for openssl-3.5.2" >&2
      exit 1
    "%,
  },

  "shadow-4.18.0" = {
    type = "binary",
    inputs = [ "shadow-src-4.18.0" ],
    outputs = [ "shadow-4.18.0" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/shadow-src-4.18.0
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/shadow-4.18.0
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/shadow-4.18.0
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/shadow-4.18.0
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/shadow-4.18.0/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/shadow-4.18.0/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/shadow-4.18.0/bin/ \;
        fi
        if [ -n "$(find /out/shadow-4.18.0 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for shadow-4.18.0" >&2
      exit 1
    "%,
  },

  "sysklogd-2.7.2" = {
    type = "binary",
    inputs = [ "sysklogd-src-2.7.2" ],
    outputs = [ "sysklogd-2.7.2" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/sysklogd-src-2.7.2
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/sysklogd-2.7.2
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/sysklogd-2.7.2
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/sysklogd-2.7.2
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/sysklogd-2.7.2/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/sysklogd-2.7.2/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/sysklogd-2.7.2/bin/ \;
        fi
        if [ -n "$(find /out/sysklogd-2.7.2 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for sysklogd-2.7.2" >&2
      exit 1
    "%,
  },

  "systemd-257.8" = {
    type = "binary",
    inputs = [ "systemd-src-257.8" ],
    outputs = [ "systemd-257.8" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/systemd-src-257.8
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/systemd-257.8
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/systemd-257.8
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/systemd-257.8
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/systemd-257.8/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/systemd-257.8/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/systemd-257.8/bin/ \;
        fi
        if [ -n "$(find /out/systemd-257.8 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for systemd-257.8" >&2
      exit 1
    "%,
  },

  "sysvinit-3.14" = {
    type = "binary",
    inputs = [ "sysvinit-src-3.14" ],
    outputs = [ "sysvinit-3.14" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/sysvinit-src-3.14
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/sysvinit-3.14
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/sysvinit-3.14
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/sysvinit-3.14
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/sysvinit-3.14/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/sysvinit-3.14/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/sysvinit-3.14/bin/ \;
        fi
        if [ -n "$(find /out/sysvinit-3.14 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for sysvinit-3.14" >&2
      exit 1
    "%,
  },

  "vim-9.1.1629" = {
    type = "binary",
    inputs = [ "vim-src-9.1.1629" ],
    outputs = [ "vim-9.1.1629" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/vim-src-9.1.1629
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/vim-9.1.1629
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/vim-9.1.1629
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/vim-9.1.1629
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/vim-9.1.1629/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/vim-9.1.1629/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/vim-9.1.1629/bin/ \;
        fi
        if [ -n "$(find /out/vim-9.1.1629 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for vim-9.1.1629" >&2
      exit 1
    "%,
  },

  "xz-5.8.1" = {
    type = "binary",
    inputs = [ "xz-src-5.8.1" ],
    outputs = [ "xz-5.8.1" ],
    script = m%"
      #!/bin/sh
      set -eu

      cd /in/xz-src-5.8.1
      jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

      if [ -f CMakeLists.txt ]; then
        cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/out/xz-5.8.1
        cmake --build build -j"$jobs"
        cmake --install build
        exit 0
      fi

      if [ -f meson.build ]; then
        meson setup build --prefix=/out/xz-5.8.1
        meson compile -C build -j"$jobs"
        meson install -C build
        exit 0
      fi

      if [ -x ./autogen.sh ]; then ./autogen.sh; fi
      if [ -x ./bootstrap ]; then ./bootstrap; fi

      if [ -x ./configure ]; then
        ./configure --prefix=/out/xz-5.8.1
        make -j"$jobs"
        make install
        exit 0
      fi

      if [ -f Makefile ] || [ -f makefile ] || [ -f GNUmakefile ]; then
        make -j"$jobs"
        if [ -d ./bin ]; then cp -a ./bin/. /out/xz-5.8.1/ 2>/dev/null || true; fi
        if [ -d ./programs ]; then
          install -d /out/xz-5.8.1/bin
          find ./programs -maxdepth 1 -type f -perm /111 -exec install -m 0755 {} /out/xz-5.8.1/bin/ \;
        fi
        if [ -n "$(find /out/xz-5.8.1 -mindepth 1 -print -quit 2>/dev/null || true )" ]; then
          exit 0
        fi
      fi

      echo "no supported build workflow for xz-5.8.1" >&2
      exit 1
    "%,
  },
}
