let store = import "./store.ncl" in
store.bind (store.fetch "source" {
  url = "__URL__",
  hash = "__SOURCE_HASH__",
  unpack = true,
}) (fun source =>
store.bind (store.text "script" {
  kind = "build-script",
  source = "#!/bin/sh\nexit 0\n",
}) (fun script =>
store.bind (store.container_image "base-image" {
  image = "docker.io/library/buildpack-deps:bookworm",
  digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
}) (fun image =>
store.bind (store.binary "binary" {
  kind = "binary-output",
  optimize = "size",
} image script [source]) (fun binary =>
store.image "final-image" {
  mode = "bootstrap",
} null [binary]))))
