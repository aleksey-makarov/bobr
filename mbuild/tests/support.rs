#![allow(dead_code)]

use bobr_store::identity::ObjectHash;
use mbuild_origin_oci_registry::fetch_image_authenticated;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub fn write_recipe(recipe_path: &Path, recipe: &Value) {
    write_recipe_with_options(recipe_path, recipe, &json!({}));
}

pub fn write_recipe_with_options(recipe_path: &Path, recipe: &Value, options: &Value) {
    if let Some(parent) = recipe_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let root = recipe_path
        .parent()
        .expect("recipe path for tests must have a parent");
    let store = store_root(root);
    fs::create_dir_all(&store).unwrap();
    let request = normalize_request(recipe);
    let envelope = json!({
        "paths": {
            "store": store.to_string_lossy(),
        },
        "options": options,
        "nodes": request,
    });
    fs::write(recipe_path, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
}

fn normalize_request(recipe: &Value) -> Value {
    fn visit(
        node: &Value,
        nodes: &mut serde_json::Map<String, Value>,
        next_id: &mut usize,
        is_root: bool,
    ) -> String {
        let id = if is_root {
            "root".to_string()
        } else {
            let id = format!("n{}", *next_id);
            *next_id += 1;
            id
        };

        let object = node.as_object().expect("recipe node must be an object");
        let name = object
            .get("name")
            .cloned()
            .expect("recipe node must have name");
        let tag = object
            .get("tag")
            .cloned()
            .expect("recipe node must have tag");

        if tag.as_str() == Some("Source") {
            let object_hash = object
                .get("object_hash")
                .cloned()
                .expect("source recipe node must have object_hash");
            let mut source_node = serde_json::Map::new();
            source_node.insert("name".to_string(), name);
            source_node.insert("tag".to_string(), tag);
            source_node.insert("object_hash".to_string(), object_hash);
            if let Some(origin) = object.get("origin").cloned() {
                source_node.insert("origin".to_string(), origin);
            }
            nodes.insert(id.clone(), Value::Object(source_node));
            return id;
        }

        let config = object
            .get("config")
            .cloned()
            .expect("recipe node must have config");
        let inputs = object
            .get("inputs")
            .and_then(Value::as_object)
            .expect("recipe node inputs must be an object");

        let mut normalized_inputs = serde_json::Map::new();
        for (slot, value) in inputs {
            normalized_inputs.insert(slot.clone(), normalize_input(value, nodes, next_id));
        }

        nodes.insert(
            id.clone(),
            json!({
                "name": name,
                "tag": tag,
                "config": config,
                "inputs": normalized_inputs,
            }),
        );

        id
    }

    fn normalize_input(
        value: &Value,
        nodes: &mut serde_json::Map<String, Value>,
        next_id: &mut usize,
    ) -> Value {
        match value {
            Value::Object(_) => Value::String(visit(value, nodes, next_id, false)),
            _ => panic!("recipe input must be an object"),
        }
    }

    let mut nodes = serde_json::Map::new();
    let mut next_id = 0usize;
    let root_id = visit(recipe, &mut nodes, &mut next_id, true);
    assert_eq!(root_id, "root");
    Value::Object(nodes)
}

fn build_ref_path(root: &Path, build_key: impl ToString) -> PathBuf {
    store_root(root).join("builds").join(build_key.to_string())
}

fn result_record_file_path(root: &Path, object_hash: ObjectHash) -> PathBuf {
    store_root(root)
        .join("results")
        .join(format!("{}.json", object_hash.to_hex()))
}

pub fn build_ref_count(root: &Path) -> usize {
    fs::read_dir(store_root(root).join("builds"))
        .unwrap()
        .count()
}

pub fn result_record_count(root: &Path) -> usize {
    fs::read_dir(store_root(root).join("results"))
        .unwrap()
        .count()
}

pub fn remove_build_ref(root: &Path, build_key: impl ToString) {
    let build_ref = build_ref_path(root, build_key);
    fs::remove_file(&build_ref).unwrap();
    assert!(!build_ref.exists());
}

pub fn remove_result_record(root: &Path, object_hash: ObjectHash) {
    let result_path = result_record_file_path(root, object_hash);
    fs::remove_file(&result_path).unwrap();
    assert!(!result_path.exists());
}

pub fn store_root(root: &Path) -> PathBuf {
    root.join("store")
}

pub fn recipe_node(name: &str, tag: &str, config: Value, inputs: Value) -> Value {
    json!({
        "name": name,
        "tag": tag,
        "config": config,
        "inputs": inputs,
    })
}

pub fn tree_file_recipe(name: &str, path: &str, text: &str, executable: bool) -> Value {
    recipe_node(
        name,
        "Tree",
        json!({
            "tree": {
                "entries": [{
                    "type": "file",
                    "path": path,
                    "text": text,
                    "executable": executable,
                }]
            }
        }),
        json!({}),
    )
}

pub fn tree_directory_recipe(name: &str) -> Value {
    recipe_node(
        name,
        "Tree",
        json!({
            "tree": {
                "entries": [
                    { "type": "dir", "path": "dev" },
                    {
                        "type": "file",
                        "path": "etc/hostname",
                        "text": "mbuild\n",
                        "executable": false,
                    },
                    {
                        "type": "file",
                        "path": "init",
                        "text": "#!/bin/sh\nexit 0\n",
                        "executable": true,
                    },
                    {
                        "type": "symlink",
                        "path": "bin",
                        "target": "usr/bin",
                    }
                ]
            },
            "install": {
                "rules": [{
                    "path": "**",
                    "attrs": {
                        "uid": 0,
                        "gid": 0,
                        "directory_mode": 493,
                        "regular_file_mode": 420,
                        "executable_file_mode": 493,
                        "symlink_mode": 511
                    }
                }]
            }
        }),
        json!({}),
    )
}

pub fn tree_symlink_recipe(name: &str) -> Value {
    recipe_node(
        name,
        "Tree",
        json!({
            "tree": {
                "entries": [
                    { "type": "dir", "path": "usr/bin" },
                    { "type": "symlink", "path": "bin", "target": "usr/bin" },
                    { "type": "symlink", "path": "etc/mtab", "target": "/proc/self/mounts" }
                ]
            },
            "install": {
                "rules": [{
                    "path": "**",
                    "attrs": {
                        "uid": 0,
                        "gid": 0,
                        "directory_mode": 493,
                        "regular_file_mode": 420,
                        "executable_file_mode": 493,
                        "symlink_mode": 511
                    }
                }]
            }
        }),
        json!({}),
    )
}

/// Spawn a minimal OCI registry server and return
/// `(server, image_ref, pinned_digest, object_hash)`.
///
/// The `image_ref` is in the form `localhost:<port>/testimage@<pinned_digest>`
/// and can be used directly in a `Source` recipe with `origin.tag =
/// "OciRegistry"`.
///
/// The server must be kept alive until the build completes.
pub fn spawn_test_oci_registry() -> (mockito::ServerGuard, String, String, String) {
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
    let object_hash = registry_layout_object_hash(&image_ref, &pinned_digest);
    (server, image_ref, pinned_digest, object_hash)
}

fn registry_layout_object_hash(image_ref: &str, pinned_digest: &str) -> String {
    let temp = tempfile::tempdir().unwrap();
    let oci_dir = temp.path().join("image");
    fs::create_dir(&oci_dir).unwrap();
    fetch_image_authenticated(image_ref, pinned_digest, &oci_dir).unwrap();
    fsobj_hash::hash_path(&oci_dir).unwrap().to_string()
}

pub fn base_image_recipe(image: &str, digest: &str, object_hash: &str) -> Value {
    json!({
        "name": "base-image",
        "tag": "Source",
        "object_hash": object_hash,
        "origin": {
            "tag": "OciRegistry",
            "image": image,
            "digest": digest,
        }
    })
}

pub fn source_recipe(url: &str, source_hash: &str) -> Value {
    json!({
        "name": "source",
        "tag": "Source",
        "object_hash": source_hash,
        "origin": {
            "tag": "Http",
            "url": url,
            "unpack": true,
        }
    })
}

pub fn group_recipe(name: &str, inputs: Vec<Value>) -> Value {
    let mut named_inputs = serde_json::Map::new();
    for (index, input) in inputs.into_iter().enumerate() {
        named_inputs.insert(format!("in{index:03}"), input);
    }
    recipe_node(name, "Group", json!({}), Value::Object(named_inputs))
}
