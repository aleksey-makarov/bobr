# flex-2.6.4 build status

- recipe is migrated to `buildscript-autotools`
- build fails in current container image because `autopoint` is missing

Observed failure:
- `autoreconf: running: autopoint --force`
- `Can't exec "autopoint": No such file or directory`

What is needed:
- provide `autopoint` (gettext tooling) in the builder image
