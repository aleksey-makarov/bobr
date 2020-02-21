{
  schema_version = "source.recipe.v1",
  type = "source.git",
  name = "zlib",
  description = "Fixed source recipe example",
  license = "Zlib",

  source = {
    url = "https://github.com/madler/zlib.git",
    commit = "51b7f2abdade71cd9bb0e7a373ef2610ec6f9daf",
    object_format = "sha1",
  },

  checkout = {
    submodules = false,
    lfs = false,
    include_git_dir = false,
  },

  outputs = {
    source_tree = {
      kind = "dir",
      hash = {
        kind = "git_tree",
        value = "9ea4c8e2f4f6f45f4f3a865f955f14cabf6b7ec8",
      },
    },
  },
}
