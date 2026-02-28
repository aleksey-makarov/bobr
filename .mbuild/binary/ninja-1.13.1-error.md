# ninja-1.13.1 build status

- recipe is migrated to `buildscript-cmake`
- build fails in hermetic container due `FetchContent` dependency download during configure/build

Observed failure:
- `Build step for googletest failed`
- CMake `FetchContent_MakeAvailable(googletest)` path fails

Why blocked now:
- network is disabled for binary builds
- dependency is not provided as an explicit input artifact yet

What is needed:
- package googletest as input artifact (or patch ninja build to avoid online fetch)
