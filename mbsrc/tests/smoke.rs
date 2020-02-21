use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn run_ok(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_mbsrc"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run mbsrc command");

    if !output.status.success() {
        panic!(
            "command failed: mbsrc {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn run_err(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_mbsrc"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run mbsrc command");

    assert!(
        !output.status.success(),
        "expected failure: mbsrc {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run git command");

    if !output.status.success() {
        panic!(
            "git command failed: git {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn create_local_repo_with_commit(root: &Path) -> (String, std::path::PathBuf) {
    let source_repo = root.join("source-repo");
    fs::create_dir_all(&source_repo).expect("failed to create source repo dir");

    git_ok(&source_repo, &["init"]);
    git_ok(&source_repo, &["config", "user.name", "mbsrc-test"]);
    git_ok(
        &source_repo,
        &["config", "user.email", "mbsrc-test@example.invalid"],
    );

    fs::write(source_repo.join("README.txt"), "hello\n").expect("failed to write test file");
    git_ok(&source_repo, &["add", "README.txt"]);
    git_ok(&source_repo, &["commit", "-m", "initial"]);

    let commit = git_ok(&source_repo, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    (commit, source_repo)
}

#[test]
fn build_materialize_build_dematerialize_smoke() {
    let workspace = TempDir::new().expect("failed to create temp workspace");
    let workspace_path = workspace.path();

    let mbuild_root = workspace_path.join(".mbuild");
    fs::create_dir_all(&mbuild_root).expect("failed to create .mbuild");

    let (commit, source_repo) = create_local_repo_with_commit(workspace_path);

    let mirror_path = mbuild_root
        .join("github")
        .join("mirrors")
        .join("example_demo.git");
    fs::create_dir_all(mirror_path.parent().expect("missing parent"))
        .expect("failed to create mirrors dir");
    git_ok(
        workspace_path,
        &[
            "clone",
            "--mirror",
            source_repo.to_string_lossy().as_ref(),
            mirror_path.to_string_lossy().as_ref(),
        ],
    );

    let config = format!(
        "{{\n  smoke = {{\n    source = {{\n      type = \"github\",\n      repo = \"https://github.com/example/demo.git\",\n      commit = \"{}\",\n    }},\n  }},\n}}\n",
        commit
    );
    fs::write(mbuild_root.join("recipes.ncl"), config).expect("failed to write recipes.ncl");

    run_ok(workspace_path, &["build", "smoke"]);
    run_ok(workspace_path, &["materialize", "smoke"]);

    let materialized_dir = mbuild_root.join("materialized").join("smoke");
    assert!(
        materialized_dir.is_dir(),
        "materialized dir was not created"
    );
    assert!(
        materialized_dir.join("README.txt").is_file(),
        "materialized file is missing"
    );

    let state_after_materialize =
        fs::read_to_string(mbuild_root.join("state.ncl")).expect("failed to read state.ncl");
    assert!(
        state_after_materialize.contains("artifact = \"smoke\"")
            && state_after_materialize.contains("materialized = true"),
        "state.ncl doesn't reflect materialized=true for smoke artifact:\n{}",
        state_after_materialize
    );

    run_ok(workspace_path, &["build", "smoke"]);
    let state_after_rebuild =
        fs::read_to_string(mbuild_root.join("state.ncl")).expect("failed to read state.ncl");
    assert!(
        state_after_rebuild.contains("materialized = true"),
        "rebuild unexpectedly reset materialized flag:\n{}",
        state_after_rebuild
    );

    run_ok(workspace_path, &["dematerialize", "smoke"]);
    assert!(
        !materialized_dir.exists(),
        "materialized dir still exists after dematerialize"
    );

    let state_after_dematerialize =
        fs::read_to_string(mbuild_root.join("state.ncl")).expect("failed to read state.ncl");
    assert!(
        state_after_dematerialize.contains("materialized = false"),
        "state.ncl doesn't reflect materialized=false after dematerialize:\n{}",
        state_after_dematerialize
    );
}

#[test]
fn materialize_fails_when_artifact_is_not_built() {
    let workspace = TempDir::new().expect("failed to create temp workspace");
    let err = run_err(workspace.path(), &["materialize", "missing"]);
    assert!(
        err.contains("is not built yet"),
        "unexpected error message:\n{}",
        err
    );
}
