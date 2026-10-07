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

#[test]
fn search_requires_configuration_before_reading_source() {
    let output = run(&[
        "search",
        "where does checkout happen",
        "/missing-root",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(value(&output)["error"]["code"], "missing_api_key");
    assert!(!output.stderr.is_empty());
}

#[test]
fn search_validates_query_limits_and_finite_positive_timeouts() {
    for args in [
        vec!["search", "", "--json"],
        vec!["search", "   ", "--json"],
        vec!["search", "query", "--limit", "0", "--json"],
        vec!["search", "query", "--limit", "101", "--json"],
        vec!["search", "query", "--timeout", "0", "--json"],
        vec!["search", "query", "--timeout", "NaN", "--json"],
        vec!["search", "query", "--timeout", "inf", "--json"],
        vec!["search", "query", "--timeout", "301", "--json"],
        vec!["search", "query", "--timeout", "0.000000000001", "--json"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(value(&output)["schema_version"], 1, "{args:?}");
        assert!(!output.stderr.is_empty());
    }
    let long = "x".repeat(4097);
    assert_eq!(run(&["search", &long, "--json"]).status.code(), Some(2));
}

#[test]
fn search_empty_scope_exercises_cli_without_contacting_provider() {
    let root = TempDir::new().unwrap();
    for flags in [vec!["--json"], vec!["--thorough", "--json"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_blink"))
            .args(["search", "query", root.path().to_str().unwrap()])
            .args(flags)
            .env("OPENAI_API_KEY", "synthetic-secret-never-print")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let report = value(&output);
        assert_eq!(report["coverage"]["complete"], true);
        assert_eq!(report["budgets"]["attempts"], 0);
        assert!(report["results"].as_array().unwrap().is_empty());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret"));
    }
}

#[test]
fn search_deadline_includes_discovery_and_json_stays_valid() {
    let root = TempDir::new().unwrap();
    for index in 0..100 {
        fs::write(
            root.path().join(format!("source{index}.rs")),
            "fn sample() {}\n",
        )
        .unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_blink"))
        .args([
            "search",
            "query",
            root.path().to_str().unwrap(),
            "--timeout",
            "0.000000001",
            "--json",
        ])
        .env("OPENAI_API_KEY", "synthetic-key")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(value(&output)["coverage"]["complete"], false);
    assert_eq!(value(&output)["budgets"]["attempts"], 0);
    assert_eq!(value(&output)["operation"], "incomplete");
}

#[test]
fn malformed_credentials_are_never_echoed() {
    let output = Command::new(env!("CARGO_BIN_EXE_blink"))
        .args(["search", "query", "--json"])
        .env("OPENAI_API_KEY", "synthetic-secret\ninvalid")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(value(&output)["error"]["code"], "invalid_api_key");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-secret"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret"));
}
