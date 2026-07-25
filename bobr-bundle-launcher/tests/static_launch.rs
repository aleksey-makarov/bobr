//! End-to-end static payload launch tests.

mod support;

use support::BundleFixture;

#[cfg(target_arch = "x86_64")]
#[test]
fn explicit_run_executes_static_payload_and_propagates_status() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_static_exit_fixture("root/usr/bin/demo", 42);

    let status = fixture
        .command()
        .args(["--run", "demo", "--", "opaque-argument"])
        .status()
        .unwrap();

    assert_eq!(status.code(), Some(42));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn public_multicall_wrapper_executes_static_payload() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_static_exit_fixture("root/usr/bin/demo", 23);
    let wrapper = fixture.add_public_wrapper("demo");

    let status = std::process::Command::new(wrapper).status().unwrap();

    assert_eq!(status.code(), Some(23));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn diagnose_reports_static_target_without_executing_it() {
    let fixture = BundleFixture::new();
    fixture.write_config("root/usr/bin/demo", "logical-demo", &[]);
    fixture.write_static_exit_fixture("root/usr/bin/demo", 42);

    let output = fixture
        .command()
        .args(["--diagnose", "demo"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("tool=demo"));
    assert!(stdout.contains("linkage=static"));
    assert!(stdout.contains("target="));
}
