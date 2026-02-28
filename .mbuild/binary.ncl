{

  "zstd-1.5.7" = {
    type = "binary",
    inputs = ["zstd-src-1.5.7", "buildscript-gnu-make"],
    outputs = ["zstd-1.5.7"],
  },

  "bc-7.0.3" = {
    type = "binary",
    inputs = ["bc-src-7.0.3", "buildscript-autotools"],
    outputs = ["bc-7.0.3"],
  },

  "expat-2.7.1" = {
    type = "binary",
    inputs = ["expat-src-2.7.1", "buildscript-cmake"],
    outputs = ["expat-2.7.1"],
  },

  "flex-2.6.4" = {
    type = "binary",
    inputs = ["flex-src-2.6.4", "buildscript-autotools"],
    outputs = ["flex-2.6.4"],
  },

  "iana-etc-20250807" = {
    type = "binary",
    inputs = ["iana-etc-src-20250807"],
    outputs = ["iana-etc-20250807"],
    script = m%"
      #!/bin/sh
      echo "TODO: migrate this recipe to buildscript-*" >&2
      exit 1
    "%,
  },

  "libffi-3.5.2" = {
    type = "binary",
    inputs = ["libffi-src-3.5.2", "buildscript-autotools"],
    outputs = ["libffi-3.5.2"],
  },

  "libxcrypt-4.4.38" = {
    type = "binary",
    inputs = ["libxcrypt-src-4.4.38", "buildscript-autotools"],
    outputs = ["libxcrypt-4.4.38"],
  },

  "lz4-1.10.0" = {
    type = "binary",
    inputs = ["lz4-src-1.10.0", "buildscript-gnu-make"],
    outputs = ["lz4-1.10.0"],
  },

  "meson-1.8.3" = {
    type = "binary",
    inputs = ["meson-src-1.8.3"],
    outputs = ["meson-1.8.3"],
    script = m%"
      #!/bin/sh
      echo "TODO: migrate this recipe to buildscript-*" >&2
      exit 1
    "%,
  },

  "ninja-1.13.1" = {
    type = "binary",
    inputs = ["ninja-src-1.13.1", "buildscript-cmake"],
    outputs = ["ninja-1.13.1"],
  },

  "openssl-3.5.2" = {
    type = "binary",
    inputs = ["openssl-src-3.5.2"],
    outputs = ["openssl-3.5.2"],
    script = m%"
      #!/bin/sh
      echo "TODO: migrate this recipe to buildscript-*" >&2
      exit 1
    "%,
  },

  "shadow-4.18.0" = {
    type = "binary",
    inputs = ["shadow-src-4.18.0", "buildscript-autotools"],
    outputs = ["shadow-4.18.0"],
  },

  "sysklogd-2.7.2" = {
    type = "binary",
    inputs = ["sysklogd-src-2.7.2", "buildscript-autotools"],
    outputs = ["sysklogd-2.7.2"],
  },

  "systemd-257.8" = {
    type = "binary",
    inputs = ["systemd-src-257.8", "buildscript-meson"],
    outputs = ["systemd-257.8"],
  },

  "sysvinit-3.14" = {
    type = "binary",
    inputs = ["sysvinit-src-3.14", "buildscript-gnu-make"],
    outputs = ["sysvinit-3.14"],
  },

  "vim-9.1.1629" = {
    type = "binary",
    inputs = ["vim-src-9.1.1629", "buildscript-autotools"],
    outputs = ["vim-9.1.1629"],
  },

  "xz-5.8.1" = {
    type = "binary",
    inputs = ["xz-src-5.8.1", "buildscript-cmake"],
    outputs = ["xz-5.8.1"],
  },
}
