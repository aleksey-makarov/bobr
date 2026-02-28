{
  "buildscript-gnu-make" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = ["buildscript-gnu-make"],
    source = import "scripts/build/gnu-make.sh" as 'Text,
  },

  "buildscript-cmake" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = ["buildscript-cmake"],
    source = import "scripts/build/cmake.sh" as 'Text,
  },

  "buildscript-autotools" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = ["buildscript-autotools"],
    source = import "scripts/build/autotools.sh" as 'Text,
  },

  "buildscript-meson" = {
    type = "text",
    artifact_kind = "build-script",
    outputs = ["buildscript-meson"],
    source = import "scripts/build/meson.sh" as 'Text,
  },
}
