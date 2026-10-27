use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/knowledge")
        .join(name)
}

fn run(catalog: &Path, output: &Path, check: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-docs"));
    command
        .arg("--catalog")
        .arg(catalog)
        .arg("--schema")
        .arg(source("catalog.schema.json"))
        .arg("render")
        .arg("--output")
        .arg(output);
    if check {
        command.arg("--check");
    }
    command.output().expect("run documentation checker")
}

#[test]
fn detects_drift_without_rewriting_and_protects_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = directory.path().join("catalog.json");
    fs::copy(source("catalog.json"), &catalog).unwrap();
    let output = directory.path().join("reference.md");
    assert!(run(&catalog, &output, false).status.success());
    assert!(run(&catalog, &output, true).status.success());
    let mut changed = fs::read(&output).unwrap();
    changed.push(b'!');
    fs::write(&output, &changed).unwrap();
    let result = run(&catalog, &output, true);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("is stale"));
    assert_eq!(fs::read(&output).unwrap(), changed);
    let original = fs::read(&catalog).unwrap();
    assert!(!run(&catalog, &catalog, false).status.success());
    assert_eq!(fs::read(&catalog).unwrap(), original);
}

#[test]
fn record_order_does_not_change_reference_output() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("first.md");
    assert!(
        run(&source("catalog.json"), &output, false)
            .status
            .success()
    );
    let expected = fs::read(&output).unwrap();
    let mut catalog: Value =
        serde_json::from_slice(&fs::read(source("catalog.json")).unwrap()).unwrap();
    for key in ["sources", "claims", "issues"] {
        catalog[key].as_array_mut().unwrap().reverse();
    }
    let input = directory.path().join("reordered.json");
    fs::write(&input, serde_json::to_vec(&catalog).unwrap()).unwrap();
    assert!(run(&input, &output, false).status.success());
    assert_eq!(fs::read(output).unwrap(), expected);
}

#[test]
fn renderer_preserves_literal_urls_and_escapes_symbol_markup() {
    let directory = tempfile::tempdir().unwrap();
    let mut catalog: Value =
        serde_json::from_slice(&fs::read(source("catalog.json")).unwrap()).unwrap();
    let source = catalog["sources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["kind"] == "source_code")
        .unwrap();
    let source_id = source["id"].clone();
    source["url"] = Value::from(format!(
        "https://example.org/{}/a)b?q=1&copy;x=2",
        source["commit"].as_str().unwrap()
    ));
    let evidence = catalog["claims"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .flat_map(|c| c["evidence"].as_array_mut().unwrap())
        .find(|e| e["source_id"] == source_id)
        .unwrap();
    evidence["locator"]["symbol"] = Value::from("x` <img src=\"https://invalid.test/a\">");
    let input = directory.path().join("markup.json");
    fs::write(&input, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let output = directory.path().join("reference.md");
    let result = run(&input, &output, false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rendered = fs::read_to_string(output).unwrap();
    assert!(!rendered.contains("<img"));
    assert!(rendered.contains("&lt;img"));
    assert!(rendered.contains("a%29b?q=1&amp;copy;x=2"));
}
