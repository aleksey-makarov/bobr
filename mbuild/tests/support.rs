#![allow(dead_code)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// Spawn a minimal OCI registry server and return
/// `(server, image_ref, pinned_digest)`.
///
/// The `image_ref` is in the form `localhost:<port>/testimage@<pinned_digest>`
/// and can be used directly in a ContainerImage recipe.
///
/// The server must be kept alive until the build completes.
pub fn spawn_test_oci_registry() -> (mockito::ServerGuard, String, String) {
    let config_bytes = b"{}";
    let config_hex = format!("{:x}", Sha256::digest(config_bytes));
    let config_digest = format!("sha256:{config_hex}");

    let layer_bytes: &[u8] = &[1, 2, 3];
    let layer_hex = format!("{:x}", Sha256::digest(layer_bytes));
    let layer_digest = format!("sha256:{layer_hex}");

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_bytes.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest,
            "size": layer_bytes.len()
        }]
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_hex = format!("{:x}", Sha256::digest(&manifest_bytes));
    let pinned_digest = format!("sha256:{manifest_hex}");

    let repo = "testimage";
    let path_manifests = format!("/v2/{repo}/manifests/{pinned_digest}");
    let path_config = format!("/v2/{repo}/blobs/{config_digest}");
    let path_layer = format!("/v2/{repo}/blobs/{layer_digest}");

    let mut server = mockito::Server::new();
    let _m1 = server.mock("GET", "/v2/").with_status(200).create();
    let _m2 = server
        .mock("GET", path_manifests.as_str())
        .with_status(200)
        .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .with_body(manifest_bytes)
        .expect_at_least(1)
        .create();
    let _m3 = server
        .mock("GET", path_config.as_str())
        .with_status(200)
        .with_body(config_bytes.as_ref())
        .expect_at_least(1)
        .create();
    let _m4 = server
        .mock("GET", path_layer.as_str())
        .with_status(200)
        .with_body(layer_bytes)
        .expect_at_least(1)
        .create();

    let image_ref = format!("{}/{repo}@{pinned_digest}", server.host_with_port());
    (server, image_ref, pinned_digest)
}

pub fn base_image_recipe(image: &str, digest: &str) -> Value {
    recipe_node(
        "base-image",
        "ContainerImage",
        json!({
            "image": image,
            "digest": digest,
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

pub fn binary_recipe(name: &str, url: &str, source_hash: &str, image: &str, digest: &str) -> Value {
    recipe_node(
        name,
        "Binary",
        json!({
            "kind": "binary-output",
        }),
        json!({
            "image": base_image_recipe(image, digest),
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
