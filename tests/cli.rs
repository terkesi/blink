use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};
use tempfile::TempDir;

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_blink"))
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap()
}
fn value(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn json_flag_works_before_and_after_all_subcommands() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.rs"), "hello\n").unwrap();
    let root = root.path().to_str().unwrap();
    for args in [
        vec!["files", root, "--json"],
        vec!["--json", "files", root],
        vec!["doctor", "--json"],
        vec!["--json", "doctor"],
        vec!["version", "--json"],
        vec!["--json", "version"],
    ] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(value(&output)["schema_version"], 1);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn files_json_reports_hash_and_coverage_without_source() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.rs"), "private source body\n").unwrap();
    fs::write(root.path().join(".env"), "credential").unwrap();
    let output = run(&["files", root.path().to_str().unwrap(), "--json"]);
    assert!(output.status.success());
    let json = value(&output);
    assert_eq!(json["files"][0]["path"], "source.rs");
    assert_eq!(json["files"][0]["bytes"], 20);
    assert_eq!(json["files"][0]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(json["coverage"]["complete"], true);
    assert_eq!(json["coverage"]["excluded"]["sensitive"], 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private source body"));
}

#[test]
fn doctor_never_prints_the_key_or_claims_authentication() {
    let output = Command::new(env!("CARGO_BIN_EXE_blink"))
        .args(["doctor", "--json"])
        .env("OPENAI_API_KEY", "synthetic-secret-never-print")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        value(&output)["configuration"]["OPENAI_API_KEY"]["present"],
        true
    );
    assert_eq!(value(&output)["provider_contacted"], false);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret-never-print"));
    assert_eq!(
        value(&run(&["doctor", "--json"]))["configuration"]["OPENAI_API_KEY"]["present"],
        false
    );
}

#[test]
fn version_matches_the_compiled_package() {
    assert_eq!(
        value(&run(&["version", "--json"]))["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        String::from_utf8(run(&["version"]).stdout).unwrap(),
        format!("blink {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn missing_roots_and_incomplete_inventory_have_distinct_statuses() {
    let root = TempDir::new().unwrap();
    let missing = run(&[
        "files",
        root.path().join("missing").to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(value(&missing)["error"]["operation"], "root_open");
    assert!(!missing.stderr.is_empty());
    fs::write(root.path().join(".gitignore"), [255]).unwrap();
    let incomplete = run(&["files", root.path().to_str().unwrap(), "--json"]);
    assert_eq!(incomplete.status.code(), Some(3));
    assert_eq!(value(&incomplete)["coverage"]["complete"], false);
    assert!(!incomplete.stderr.is_empty());
}

#[test]
fn terminal_paths_escape_control_characters() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("a\nb.rs"), "text").unwrap();
    let output = run(&["files", root.path().to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "\"a\\nb.rs\"\n");
    assert!(!output.stderr.is_empty());
}
