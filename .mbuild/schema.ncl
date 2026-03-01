let ArtifactName = String in
let NameList = Array ArtifactName in

let BaseRecipeContract = {
  type | String,
  inputs | NameList | optional,
  outputs | NameList | optional,
  ..
}
in

let GithubRecipeContract = {
  type | std.contract.Equal "github",
  repo | String,
  commit | String,
  inputs | NameList | optional,
  outputs | NameList | optional,
  ..
}
in

let BinaryRecipeContract = {
  type | std.contract.Equal "binary",
  script | String | optional,
  inputs | NameList | optional,
  outputs | NameList | optional,
  ..
}
in

let TextRecipeContract = {
  type | std.contract.Equal "text",
  artifact_kind | String,
  source | String | optional,
  sources | { _ | String } | optional,
  outputs | NameList | optional,
  ..
}
in

let FetchRecipeContract = {
  type | std.contract.Equal "fetch",
  url | String,
  hash | String,
  layout | std.contract.Enum [
    "file",
    "archive-tree",
  ],
  archive_format | std.contract.Enum [
    "tar-gz",
    "tar-xz",
    "zip",
  ]
    | optional,
  artifact_kind | String | optional,
  outputs | NameList | optional,
  ..
}
in

{
  validate_recipe = fun recipe =>
    let base = recipe | BaseRecipeContract in
    if base.type == "github" then
      base | GithubRecipeContract
    else if base.type == "binary" then
      base | BinaryRecipeContract
    else if base.type == "text" then
      base | TextRecipeContract
    else if base.type == "fetch" then
      base | FetchRecipeContract
    else
      base,
}
