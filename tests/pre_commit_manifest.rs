use serde_json::Value;
use std::path::PathBuf;

#[test]
fn pre_commit_manifest_runs_attest_against_the_staged_index() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(root.join(".pre-commit-hooks.yaml")).unwrap();
    let hooks: Vec<Value> = serde_yaml_ng::from_slice(&bytes).unwrap();
    let hook = &hooks[0];

    assert_eq!(hook["id"], "attest");
    assert_eq!(hook["entry"], "attest pre-commit-hook");
    assert_eq!(hook["language"], "rust");
    assert_eq!(hook["pass_filenames"], false);
    assert_eq!(hook["always_run"], true);
    assert_eq!(hook["require_serial"], true);
    assert_eq!(hook["stages"][0], "pre-commit");
}
