{
  "buildscript-gnu-make" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = [ "buildscript-gnu-make" ],
    source = "scripts/build/gnu-make.sh",
  },

  "buildscript-cmake" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = [ "buildscript-cmake" ],
    source = "scripts/build/cmake.sh",
  },

  "buildscript-autotools" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = [ "buildscript-autotools" ],
    source = "scripts/build/autotools.sh",
  },

  "buildscript-meson" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = [ "buildscript-meson" ],
    source = "scripts/build/meson.sh",
  },
}
