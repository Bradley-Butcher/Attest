use assert_cmd::Command;
use attest_contracts::{ClaimStatus, ReviewFile};
use camino::Utf8PathBuf;
use predicates::prelude::*;
use std::process::Command as StdCommand;
use tempfile::TempDir;

#[test]
fn nested_change_requires_root_and_directory_contracts() {
    let repo = TestRepo::new();
    repo.write(
        "AGENT_CONTRACT.yaml",
        r#"version: 1
module: repo
claims:
  - id: repo.reviewed
    text: I reviewed the whole repo impact.
"#,
    );
    repo.write(
        "crates/engine/AGENT_CONTRACT.yaml",
        r#"version: 1
module: engine
claims:
  - id: engine.no_test_case_heuristics
    text: I did not add test-case-specific engine logic.
"#,
    );
    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 41 }\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "initial"]);

    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "update engine"]);

    Command::cargo_bin("attest")
        .unwrap()
        .args(["review-pr", "--repo", repo.path(), "--base", "HEAD~1"])
        .assert()
        .success();

    let review_path = repo.root.join(".git/attest/pr-review.yaml");
    let review_bytes = std::fs::read(&review_path).unwrap();
    let mut review: ReviewFile = serde_yaml_ng::from_slice(&review_bytes).unwrap();
    let mut claim_ids = review
        .items
        .iter()
        .map(|item| item.claim_id.clone())
        .collect::<Vec<_>>();
    claim_ids.sort();
    assert_eq!(
        claim_ids,
        vec![
            "engine.no_test_case_heuristics".to_string(),
            "repo.reviewed".to_string()
        ]
    );

    for item in &mut review.items {
        item.status = Some(ClaimStatus::True);
        item.evidence = vec![format!("reviewed {}", item.claim_id)];
    }
    std::fs::write(&review_path, serde_yaml_ng::to_string(&review).unwrap()).unwrap();

    Command::cargo_bin("attest")
        .unwrap()
        .args([
            "sign-pr",
            "--repo",
            repo.path(),
            "--base",
            "HEAD~1",
            "--from-review",
        ])
        .assert()
        .success();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["verify-pr", "--repo", repo.path(), "--base", "HEAD~1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("accepted"));

    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 43 }\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "change after signing"]);

    Command::cargo_bin("attest")
        .unwrap()
        .args(["verify-pr", "--repo", repo.path(), "--base", "HEAD~2"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("head sha is stale"));

    Command::cargo_bin("attest")
        .unwrap()
        .args(["codex-stop-hook", "--repo", repo.path(), "--base", "HEAD~2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"continue\": false"))
        .stdout(predicate::str::contains("Do not sign mechanically"));
}

#[test]
fn inline_function_contract_activates_only_for_function_changes() {
    let repo = TestRepo::new();
    repo.write(
        "src/lib.rs",
        r#"// attest: begin
// scope: function
// id: engine.guarded
// module: engine
// claims:
//   - id: engine.no_test_case_heuristics
//     text: guarded does not add fixture-specific logic.
// attest: end
pub fn guarded(input: u32) -> u32 {
    input + 1
}

pub fn other() -> u32 {
    0
}
"#,
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "initial inline contract"]);

    Command::cargo_bin("attest")
        .unwrap()
        .args(["inline", "--repo", repo.path(), "explain", "src/lib.rs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("engine.guarded"))
        .stdout(predicate::str::contains("function guarded"));

    repo.write(
        "src/lib.rs",
        r#"use std::fmt;

// attest: begin
// scope: function
// id: engine.guarded
// module: engine
// claims:
//   - id: engine.no_test_case_heuristics
//     text: guarded does not add fixture-specific logic.
// attest: end
pub fn guarded(input: u32) -> u32 {
    input + 1
}

pub fn other() -> u32 {
    0
}
"#,
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "change import"]);

    Command::cargo_bin("attest")
        .unwrap()
        .args([
            "status",
            "--repo",
            repo.path(),
            "--base",
            "HEAD~1",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("engine.no_test_case_heuristics").not());

    repo.write(
        "src/lib.rs",
        r#"use std::fmt;

// attest: begin
// scope: function
// id: engine.guarded
// module: engine
// claims:
//   - id: engine.no_test_case_heuristics
//     text: guarded does not add fixture-specific logic.
// attest: end
pub fn guarded(input: u32) -> u32 {
    if input == 42 {
        return 42;
    }
    input + 1
}

pub fn other() -> u32 {
    0
}
"#,
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "change guarded"]);

    Command::cargo_bin("attest")
        .unwrap()
        .args(["review-pr", "--repo", repo.path(), "--base", "HEAD~1"])
        .assert()
        .success();

    let review_path = repo.root.join(".git/attest/pr-review.yaml");
    let review_bytes = std::fs::read(&review_path).unwrap();
    let review: ReviewFile = serde_yaml_ng::from_slice(&review_bytes).unwrap();
    assert_eq!(review.items.len(), 1);
    let item = &review.items[0];
    assert_eq!(
        item.contract_path.as_str(),
        "src/lib.rs#attest:engine.guarded"
    );
    assert_eq!(item.claim_id, "engine.no_test_case_heuristics");
    let target = item.target.as_ref().unwrap();
    assert_eq!(target.symbol.as_deref(), Some("guarded"));
}

struct TestRepo {
    _tmp: TempDir,
    root: Utf8PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let repo = Self { _tmp: tmp, root };
        repo.git(["init", "-b", "main"]);
        repo.git(["config", "user.email", "attest@example.com"]);
        repo.git(["config", "user.name", "Attest Test"]);
        repo.git(["config", "commit.gpgsign", "false"]);
        repo.git(["config", "tag.gpgsign", "false"]);
        repo
    }

    fn path(&self) -> &str {
        self.root.as_str()
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn git<const N: usize>(&self, args: [&str; N]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
