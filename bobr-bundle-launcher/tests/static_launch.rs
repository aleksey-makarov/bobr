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
    assert!(stdout.contains("visibility=public"));
    assert!(stdout.contains("platform.compatible=true"));
    assert!(stdout.contains("library_path="));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn json_diagnostics_report_platform_policy_and_environment_origin() {
    let fixture = BundleFixture::new();
    fixture.write_config_with_environment(
        "root/usr/bin/demo",
        "logical-demo",
        &[],
        r#"
[environment.QEMU_AUDIO_DRV]
operation = "default"
values = ["none"]
"#,
    );
    fixture.write_static_exit_fixture("root/usr/bin/demo", 42);

    let output = fixture
        .command()
        .args(["--diagnose", "demo", "--json"])
        .env_remove("QEMU_AUDIO_DRV")
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["policy"], "strict");
    assert_eq!(report["platform"]["compatible"], true);
    assert_eq!(report["tool"]["visibility"], "public");
    assert_eq!(report["executable"]["linkage"], "static");
    let audio = report["environment"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "QEMU_AUDIO_DRV")
        .unwrap();
    assert_eq!(audio["value"], "none");
    assert_eq!(audio["origin"], "common:default");
}
