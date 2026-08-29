#![allow(missing_docs)]
mod support;

use bobr_core::{BuildKey, ObjectHash, ReuseKey};
use serde_json::json;
use std::fs;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};
use support::{
    TEST_RUN_ID, make_run_dirs, recipe_node, store_root, tree_file_recipe, write_request,
    write_request_with_options,
};
use tempfile::tempdir;

#[test]
fn cli_reports_its_version_and_request_schema() {
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
            .arg(flag)
            .output()
            .unwrap();

        assert!(output.status.success(), "{flag} failed");
        let line = String::from_utf8(output.stdout).unwrap();
        let line = line.trim();
        // Two answers in one line: which build this is, and which requests it
        // accepts.
        assert!(
            line.starts_with(&format!("bobr {}", env!("CARGO_PKG_VERSION"))),
            "{line}"
        );
        assert!(
            line.ends_with(&format!("(request {})", bobr::REQUEST_SCHEMA)),
            "{line}"
        );
    }
}

#[test]
fn cli_reads_request_from_stdin_when_path_is_omitted() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("stdin.json");
    write_request(
        &request_path,
        &tree_file_recipe("stdin-recipe", "stdin.txt", "hello stdin", false),
    );
    let request_bytes = fs::read(&request_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.as_mut().unwrap().write_all(&request_bytes)?;
            child.wait_with_output()
        })
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _object_hash: ObjectHash = stdout.trim().parse().unwrap();
    assert!(
        stderr.contains("Tree stdin-recipe: starting subject"),
        "{stderr}"
    );
    assert!(stderr.contains("subject completed"), "{stderr}");
}

#[test]
fn cli_accepts_explicit_request_path() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("custom.json");
    write_request(
        &request_path,
        &tree_file_recipe("custom-recipe", "custom.txt", "hello custom", false),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _object_hash: ObjectHash = stdout.trim().parse().unwrap();
    assert!(
        stderr.contains("Tree custom-recipe: starting subject"),
        "{stderr}"
    );
    let events = fs::read_to_string(
        store_root(workspace.path())
            .join("logs")
            .join(TEST_RUN_ID)
            .join("events.jsonl"),
    )
    .unwrap();
    let parsed = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let started = parsed
        .iter()
        .find(|event| event["status"] == "run-started")
        .expect("unified Realizer must record a run-started event");
    assert_eq!(started["details"]["reachable"], 1);
    assert_eq!(started["details"]["reachable_builders"], 1);
    assert_eq!(started["details"]["reachable_sources"], 0);
    assert_eq!(started["details"]["progress_policy"]["mode"], "auto");
    let finished = parsed
        .iter()
        .find(|event| event["status"] == "run-finished")
        .expect("unified Realizer must record a run-finished event");
    assert_eq!(finished["details"]["built"], 1);
    assert_eq!(finished["details"]["cache_hit"], 0);
    assert_eq!(finished["details"]["downloaded"], 0);
    assert_eq!(finished["details"]["failed"], 0);
}

#[test]
fn warm_unified_run_reports_one_cache_hit_without_rebuilding() {
    let workspace = tempdir().unwrap();
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    let recipe = tree_file_recipe("warm", "warm.txt", "warm", false);
    let mut hashes = Vec::new();
    for run_id in ["cold", "warm"] {
        let logs = workspace.path().join("logs").join(run_id);
        let work = workspace.path().join("work").join(run_id);
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&work).unwrap();
        let request = workspace.path().join(format!("{run_id}.json"));
        fs::write(
            &request,
            serde_json::to_vec_pretty(&json!({
                "schema": "bobr-request-v5",
                "store": &store,
                "logs": logs,
                "work": work,
                "run_id": run_id,
                "goals": ["root"],
                "nodes": { "root": recipe.clone() },
            }))
            .unwrap(),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
            .arg(request)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        hashes.push(String::from_utf8(output.stdout).unwrap());
    }
    assert_eq!(hashes[0], hashes[1]);

    let events = fs::read_to_string(
        workspace
            .path()
            .join("logs")
            .join("warm")
            .join("events.jsonl"),
    )
    .unwrap();
    let finished = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["status"] == "run-finished")
        .unwrap();
    assert_eq!(finished["details"]["built"], 0);
    assert_eq!(finished["details"]["cache_hit"], 1);
    assert_eq!(finished["details"]["failed"], 0);
    assert!(
        fs::read_dir(workspace.path().join("work").join("warm"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn cli_outputs_multi_goal_results_in_request_order() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    let request_path = workspace.path().join("multi-goal.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": store,
            "logs": logs,
            "work": work,
            "run_id": "multi-goal",
            "goals": ["second", "first"],
            "nodes": {
                "first": tree_file_recipe("first", "first.txt", "same", false),
                "second": tree_file_recipe("second", "second.txt", "same", false),
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let results: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["node"], "second");
    assert_eq!(results[1]["node"], "first");
    assert_eq!(results[0]["object_hash"], results[1]["object_hash"]);
    for result in results {
        let _: ObjectHash = result["object_hash"].as_str().unwrap().parse().unwrap();
    }
}

#[test]
fn cli_materializes_source_origin_without_a_fetch_phase() {
    let workspace = tempdir().unwrap();
    let source_path = workspace.path().join("source.txt");
    fs::write(&source_path, b"unified source\n").unwrap();
    let object_hash = fsobj_hash::hash_path(&source_path).unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    let request_path = workspace.path().join("source.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": store,
            "logs": logs,
            "work": work,
            "run_id": "source-origin",
            "goals": ["source"],
            "nodes": {
                "source": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": object_hash,
                    "origin": {
                        "tag": "Path",
                        "path": source_path,
                        "unpack": false,
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        object_hash.to_string()
    );
    let layout = bobr_store::Store::create(&store_root(workspace.path())).unwrap();
    assert!(layout.object_is_complete(object_hash).unwrap());
}

#[test]
fn cli_uses_a_trusted_hardlink_local_repository() {
    let workspace = tempdir().unwrap();
    let secondary_root = workspace.path().join("secondary");
    let working_root = store_root(workspace.path());
    fs::create_dir(&secondary_root).unwrap();
    fs::create_dir_all(&working_root).unwrap();
    let secondary = bobr_store::Store::create(&secondary_root).unwrap();
    let staged = workspace.path().join("secondary-source");
    fs::write(&staged, b"secondary source\n").unwrap();
    let object_hash = fsobj_hash::hash_path(&staged).unwrap();
    bobr_store::import_build(
        &secondary,
        BuildKey::from_object_hash(object_hash),
        "1".repeat(64).parse::<ReuseKey>().unwrap(),
        Vec::new(),
        &staged,
        "secondary-source",
        "secondary-run",
    )
    .unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("secondary.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": working_root,
            "logs": &logs,
            "work": work,
            "run_id": "secondary",
            "goals": ["source"],
            "secondaries": {
                "local_repositories": [{
                    "name": "old",
                    "store": &secondary_root,
                    "trusted": true,
                    "transfer": "hardlink"
                }]
            },
            "nodes": {
                "source": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": object_hash
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        object_hash.to_string()
    );
    let working = bobr_store::Store::create(&store_root(workspace.path())).unwrap();
    assert!(working.object_is_complete(object_hash).unwrap());
    let started = fs::read_to_string(logs.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["status"] == "run-started")
        .unwrap();
    assert_eq!(
        started["details"]["local_repositories"],
        json!([{
            "name": "old",
            "store": fs::canonicalize(secondary_root).unwrap(),
            "trusted": true,
            "transfer": "hardlink"
        }])
    );
}

#[test]
fn cli_rejects_a_repository_that_aliases_the_working_store() {
    let workspace = tempdir().unwrap();
    let working_root = store_root(workspace.path());
    fs::create_dir_all(&working_root).unwrap();
    bobr_store::Store::create(&working_root).unwrap();
    let alias = workspace.path().join("working-alias");
    symlink(&working_root, &alias).unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("working-alias.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": working_root,
            "logs": logs,
            "work": work,
            "run_id": "working-alias",
            "goals": ["source"],
            "secondaries": {
                "local_repositories": [{
                    "name": "alias",
                    "store": alias,
                    "trusted": false,
                    "transfer": "hardlink"
                }]
            },
            "nodes": {
                "source": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1".repeat(64)
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("canonical alias of the working store"),
        "{stderr}"
    );
}

#[test]
fn cli_rejects_duplicate_canonical_repository_roots() {
    let workspace = tempdir().unwrap();
    let working_root = store_root(workspace.path());
    let repository_root = workspace.path().join("repository");
    fs::create_dir_all(&working_root).unwrap();
    fs::create_dir(&repository_root).unwrap();
    bobr_store::Store::create(&repository_root).unwrap();
    let alias = workspace.path().join("repository-alias");
    symlink(&repository_root, &alias).unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("duplicate-repository.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": working_root,
            "logs": logs,
            "work": work,
            "run_id": "duplicate-repository",
            "goals": ["source"],
            "secondaries": {
                "local_repositories": [
                    {
                        "name": "repository",
                        "store": repository_root,
                        "trusted": false,
                        "transfer": "hardlink"
                    },
                    {
                        "name": "alias",
                        "store": alias,
                        "trusted": false,
                        "transfer": "hardlink"
                    }
                ]
            },
            "nodes": {
                "source": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1".repeat(64)
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("resolve to the same store root"),
        "{stderr}"
    );
}

#[test]
fn ordinary_goal_failure_is_not_reported_as_cancellation() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    let request_path = workspace.path().join("missing-source.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": store,
            "logs": logs,
            "work": work,
            "run_id": "missing-source",
            "goals": ["source"],
            "nodes": {
                "source": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1".repeat(64)
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]"), "{stderr}");
    assert!(!stderr.contains("error[cancelled]"), "{stderr}");
}

#[test]
fn cli_rejects_more_than_one_request_argument() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("one.json");
    let extra_path = workspace.path().join("two.json");
    write_request(
        &request_path,
        &tree_file_recipe("one-recipe", "one.txt", "hello", false),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .arg(&extra_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(stderr.contains("unexpected argument"), "{stderr}");
    assert!(
        stderr.contains("usage: bobr [--version] [request.json]"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_missing_store_option() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("missing-store-option.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "goals": ["root"],
            "nodes": {
                "root": tree_file_recipe("missing-store-option", "missing.txt", "hello", false)
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(stderr.contains("missing field `store`"), "{stderr}");
}

#[test]
fn request_quiet_suppresses_live_progress() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("quiet.json");
    write_request_with_options(
        &request_path,
        &tree_file_recipe("quiet-recipe", "quiet.txt", "hello quiet", false),
        &json!({
            "quiet": true,
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _object_hash: ObjectHash = stdout.trim().parse().unwrap();
}

#[test]
fn request_jobs_zero_is_rejected() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("zero-jobs.json");
    write_request_with_options(
        &request_path,
        &tree_file_recipe("zero-jobs-recipe", "zero.txt", "hello zero", false),
        &json!({
            "jobs": 0,
        }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("request 'jobs' must be greater than zero"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_invalid_request() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("broken.json");
    fs::write(&request_path, b"{ not valid json").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("failed to decode request JSON value"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_invalid_generic_input_shape() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("broken-shape.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": store.to_string_lossy(),
            "logs": logs.to_string_lossy(),
            "work": work.to_string_lossy(),
            "run_id": "test-run",
            "goals": ["root"],
            "nodes": {
                "root": {
                    "name": "sandbox",
                    "tag": "Sandbox",
                    "config": {},
                    "inputs": {
                        "rootfs": []
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(stderr.contains("expected node id string"), "{stderr}");
}

#[test]
fn cli_reports_relative_store_path() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("relative-store.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": "relative/store",
            "logs": logs.to_string_lossy(),
            "work": work.to_string_lossy(),
            "run_id": "test-run",
            "goals": ["root"],
            "nodes": {
                "root": {
                    "name": "tree",
                    "tag": "Tree",
                    "config": {
                        "tree": {
                            "entries": [{
                                "type": "file",
                                "path": "hello.txt",
                                "text": "hello",
                                "executable": false
                            }]
                        }
                    },
                    "inputs": {}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]"), "{stderr}");
    assert!(stderr.contains("store root must be absolute"), "{stderr}");
}

#[test]
fn cli_reports_unexpected_local_path() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("unexpected-local.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": store.to_string_lossy(),
            "logs": logs.to_string_lossy(),
            "work": work.to_string_lossy(),
            "run_id": "test-run",
            "goals": ["root"],
            "local": "relative/local",
            "nodes": {
                "root": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "origin": {
                        "tag": "Path",
                        "path": "/tmp/payload.txt"
                    },
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown field `local`"), "{stderr}");
}

#[test]
fn cli_reports_relative_source_path() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("relative-source-path.json");
    let store = store_root(workspace.path());
    fs::create_dir_all(&store).unwrap();
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": store.to_string_lossy(),
            "logs": logs.to_string_lossy(),
            "work": work.to_string_lossy(),
            "run_id": "test-run",
            "goals": ["root"],
            "nodes": {
                "root": {
                    "name": "source",
                    "tag": "Source",
                    "object_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "origin": {
                        "tag": "Path",
                        "path": "payload.txt"
                    },
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("$.nodes.root: origin.path: expected absolute path"),
        "{stderr}"
    );
}

#[test]
fn cli_reports_missing_store_directory() {
    let workspace = tempdir().unwrap();
    let (logs, work) = make_run_dirs(workspace.path());
    let request_path = workspace.path().join("missing-store.json");
    let missing_store = workspace.path().join("missing-store");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "bobr-request-v5",
            "store": missing_store.to_string_lossy(),
            "logs": logs.to_string_lossy(),
            "work": work.to_string_lossy(),
            "run_id": "test-run",
            "goals": ["root"],
            "nodes": {
                "root": {
                    "name": "tree",
                    "tag": "Tree",
                    "config": {
                        "tree": {
                            "entries": [{
                                "type": "file",
                                "path": "hello.txt",
                                "text": "hello",
                                "executable": false
                            }]
                        }
                    },
                    "inputs": {}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[build-failed]"), "{stderr}");
    assert!(stderr.contains("store root must exist"), "{stderr}");
}

#[test]
fn cli_reports_unknown_input_slot() {
    let workspace = tempdir().unwrap();
    let request_path = workspace.path().join("unknown-slot.json");
    let recipe = recipe_node(
        "tree",
        "Tree",
        json!({
            "tree": {
                "entries": [{
                    "type": "file",
                    "path": "hello.txt",
                    "text": "hello",
                    "executable": false
                }]
            }
        }),
        json!({
            "unexpected": tree_file_recipe("dep", "dep.txt", "hello", false)
        }),
    );
    write_request(&request_path, &recipe);

    let output = Command::new(env!("CARGO_BIN_EXE_bobr"))
        .arg(&request_path)
        .current_dir(workspace.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[invalid-input]"), "{stderr}");
    assert!(
        stderr.contains("does not accept extra input 'unexpected'"),
        "{stderr}"
    );
}
