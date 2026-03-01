let schema = import "schema.ncl" in
let github = import "github.ncl" in
let binary = import "binary.ncl" in
let text = import "text.ncl" in
let fetch = import "fetch.ncl" in
let merged = github & binary & text & fetch in
std.record.map (fun _ recipe => schema.validate_recipe recipe) merged
