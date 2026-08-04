//! End-to-end bundled dynamic-loader launch tests.

mod support;

use support::GuardedCommand;

use support::BundleFixture;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn dynamic_payload_runs_through_bundled_loader() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &["root/usr/lib64"]);
    fixture.write_dynamic_fixture("root/usr/bin/demo", "/lib64/test-loader");
    fixture.write_static_exit_fixture("root/lib64/test-loader", 37);

    let status = fixture
        .command()
        .args(["--run", "demo", "--", "payload-argument"])
        .guarded_status()
        .unwrap();

    assert_eq!(status.code(), Some(37));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn diagnose_reports_bundled_loader_without_running_it() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &["root/usr/lib64"]);
    fixture.write_dynamic_fixture("root/usr/bin/demo", "/lib64/test-loader");
    fixture.write_static_exit_fixture("root/lib64/test-loader", 37);

    let output = fixture
        .command()
        .args(["--diagnose", "demo"])
        .guarded_output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("linkage=dynamic"));
    assert!(stdout.contains("loader="));
    assert!(stdout.contains("root/lib64/test-loader"));
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn missing_bundled_loader_is_an_error_without_host_fallback() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &["root/usr/lib64"]);
    fixture.write_dynamic_fixture("root/usr/bin/demo", "/lib64/definitely-missing-loader");

    let output = fixture
        .command()
        .args(["--run", "demo", "--"])
        .guarded_output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("definitely-missing-loader"));
    assert!(stderr.contains("failed to resolve ELF interpreter"));
}
