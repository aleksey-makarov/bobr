let schema = import "schema.ncl" in
let github = import "github.ncl" in
let binary = import "binary.ncl" in
let merged = github & binary in
std.record.map (fun _ recipe => schema.validate_recipe recipe) merged
