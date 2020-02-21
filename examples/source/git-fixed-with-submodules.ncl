{
  schema_version = "source.recipe.v1",
  type = "source.git",
  name = "example-with-submodules",
  description = "Fixed recipe with submodules enabled",

  source = {
    url = "git@github.com:org/repo.git",
    commit = "0123456789abcdef0123456789abcdef01234567",
    object_format = "sha1",
  },

  checkout = {
    submodules = true,
    lfs = false,
    include_git_dir = false,
  },

  outputs = {
    source_tree = {
      kind = "dir",
      hash = {
        kind = "git_tree",
        value = "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
      },
    },
  },

  provenance_policy = {
    store_logs = true,
    store_env = true,
  },
}
