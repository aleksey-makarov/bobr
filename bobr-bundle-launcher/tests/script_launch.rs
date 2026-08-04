//! End-to-end shebang and internal-wrapper tests.

mod support;

use support::GuardedCommand;

use support::BundleFixture;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn script_runs_its_bundled_static_interpreter() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_script_fixture(
        "root/usr/bin/demo",
        b"#!/bin/test-interpreter optional argument\nignored body\n",
    );
    fixture.write_static_exit_fixture("root/bin/test-interpreter", 51);

    let status = fixture
        .command()
        .args(["--run", "demo", "--", "payload-argument"])
        .guarded_status()
        .unwrap();

    assert_eq!(status.code(), Some(51));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn script_runs_a_dynamic_interpreter_through_the_bundled_loader() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &["root/usr/lib64"]);
    fixture.write_script_fixture("root/usr/bin/demo", b"#!/bin/test-interpreter\n");
    fixture.write_dynamic_fixture("root/bin/test-interpreter", "/lib64/test-loader");
    fixture.write_static_exit_fixture("root/lib64/test-loader", 52);

    let status = fixture
        .command()
        .args(["--run", "demo", "--"])
        .guarded_status()
        .unwrap();

    assert_eq!(status.code(), Some(52));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn script_never_falls_back_to_a_host_interpreter() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_script_fixture("root/usr/bin/demo", b"#!/bin/sh\nexit 0\n");

    let output = fixture
        .command()
        .args(["--run", "demo", "--"])
        .guarded_output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("bundled shebang interpreter '/bin/sh'"));
    assert!(stderr.contains("root/bin/sh"));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn nested_shebangs_are_resolved_inside_the_payload() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_script_fixture("root/usr/bin/demo", b"#!/bin/first\n");
    fixture.write_script_fixture("root/bin/first", b"#!/bin/final nested-argument\n");
    fixture.write_static_exit_fixture("root/bin/final", 53);

    let status = fixture
        .command()
        .args(["--run", "demo", "--"])
        .guarded_status()
        .unwrap();

    assert_eq!(status.code(), Some(53));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn internal_multicall_wrapper_selects_its_declared_tool() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_static_exit_fixture("root/usr/bin/demo", 54);
    let wrapper = fixture.add_internal_wrapper("demo");

    let status = std::process::Command::new(wrapper)
        .arg("payload-argument")
        .guarded_status()
        .unwrap();

    assert_eq!(status.code(), Some(54));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn diagnose_reports_script_and_logical_interpreter() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_script_fixture("root/usr/bin/demo", b"#!/bin/test-interpreter\n");
    fixture.write_static_exit_fixture("root/bin/test-interpreter", 55);

    let output = fixture
        .command()
        .args(["--diagnose", "demo"])
        .guarded_output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("linkage=script"));
    assert!(stdout.contains("interpreter=/bin/test-interpreter"));
}
