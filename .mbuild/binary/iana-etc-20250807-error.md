# iana-etc-20250807 build status

- recipe is still inline (not migrated to buildscript)
- source tree has no standard build markers (`CMakeLists.txt`, `meson.build`, `configure`, `Makefile`)

Why blocked now:
- current generic buildscript set cannot build this artifact
- likely this artifact is data/package-content oriented and needs a custom packaging script

What is needed:
- add a dedicated build-script artifact for iana-etc packaging
