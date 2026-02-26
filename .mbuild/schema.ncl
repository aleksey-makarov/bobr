let ArtifactName = String in
let NameList = Array ArtifactName in

let BaseRecipeContract = {
  type | String,
  inputs | NameList | optional,
  outputs | NameList | optional,
  ..
} in

let GithubRecipeContract = {
  type | std.contract.Equal "github",
  repo | String,
  commit | String,
  inputs | NameList | optional,
  outputs | NameList | optional,
  ..
} in

let BinaryRecipeContract = {
  type | std.contract.Equal "binary",
  script | String,
  inputs | NameList | optional,
  outputs | NameList | optional,
  ..
} in

{
  validate_recipe = fun recipe =>
    let base = recipe | BaseRecipeContract in
    if base.type == "github" then
      base | GithubRecipeContract
    else if base.type == "binary" then
      base | BinaryRecipeContract
    else
      base,
}
