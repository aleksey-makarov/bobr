{
  # Source Builder Recipe v1 (fixed git version)
  schema_version | "source.recipe.v1",
  type | "source.git",

  name | String,
  description ? String,
  license ? String,

  source = {
    url | String,
    commit | String,
    object_format | "sha1" | "sha256",
  },

  checkout = {
    submodules | Bool,
    lfs | Bool,
    include_git_dir | Bool,
  },

  outputs = {
    source_tree = {
      kind | "dir",
      hash = {
        kind | "git_tree",
        value | String,
      },
    },
  },

  provenance_policy ? {
    store_logs ? true,
    store_env ? true,
  },
}
