#![allow(dead_code)]

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub fn write_recipe(recipe_path: &Path, recipe: &Value) {
    if let Some(parent) = recipe_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(recipe_path, serde_json::to_vec_pretty(recipe).unwrap()).unwrap();
}

pub fn build_ref_path(root: &Path, build_key: impl ToString) -> PathBuf {
    root.join(".mbuild")
        .join("builds")
        .join(build_key.to_string())
}

pub fn recipe_node(name: &str, tag: &str, config: Value, inputs: Value) -> Value {
    json!({
        "name": name,
        "tag": tag,
        "config": config,
        "inputs": inputs,
    })
}

pub fn text_recipe(name: &str, kind: &str, source: &str) -> Value {
    recipe_node(
        name,
        "Text",
        json!({
            "kind": kind,
            "source": source,
        }),
        json!({}),
    )
}

pub fn base_image_recipe() -> Value {
    recipe_node(
        "base-image",
        "ContainerImage",
        json!({
            "image": "docker.io/library/buildpack-deps:bookworm",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }),
        json!({}),
    )
}

pub fn script_recipe() -> Value {
    text_recipe("script", "build-script", "#!/bin/sh\nexit 0\n")
}

pub fn source_recipe(url: &str, source_hash: &str) -> Value {
    recipe_node(
        "source",
        "Fetch",
        json!({
            "url": url,
            "hash": source_hash,
            "unpack": true,
        }),
        json!({}),
    )
}

pub fn binary_recipe(name: &str, url: &str, source_hash: &str) -> Value {
    recipe_node(
        name,
        "Binary",
        json!({
            "kind": "binary-output",
            "optimize": "size",
        }),
        json!({
            "image": base_image_recipe(),
            "script": script_recipe(),
            "sources": [source_recipe(url, source_hash)],
        }),
    )
}

pub fn image_recipe(name: &str, inputs: Vec<Value>) -> Value {
    recipe_node(
        name,
        "Image",
        json!({ "mode": "bootstrap" }),
        json!({
            "base": null,
            "inputs": inputs,
        }),
    )
}
