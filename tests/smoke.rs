use assert_cmd::Command;
use attest_contracts::{ClaimStatus, CommitAttestation};
use camino::Utf8PathBuf;
use jiff::Timestamp;
use predicates::prelude::*;
use std::process::Command as StdCommand;
use tempfile::TempDir;

#[test]
fn pre_commit_hook_writes_draft_and_blocks_until_signed() {
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
    repo.write("src/lib.rs", "pub fn answer() -> u32 { 41 }\n");
    repo.commit_all("initial");

    repo.write("src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["pre-commit-hook", "--repo", repo.path()])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "You need to sign these Attest contracts before committing",
        ))
        .stderr(predicate::str::contains("AGENT_CONTRACT.yaml"))
        .stderr(predicate::str::contains("repo.reviewed"))
        .stderr(predicate::str::contains(
            ".git/attest/pending-attestation.yaml",
        ))
        .stderr(predicate::str::contains("git commit"));

    let mut attestation = repo.read_attestation();
    assert!(attestation.signoff.signed_at.is_none());
    sign_all(&mut attestation);
    repo.write_attestation(&attestation);

    Command::cargo_bin("attest")
        .unwrap()
        .args(["verify", "--repo", repo.path()])
        .assert()
        .success()
        .stdout(predicate::str::contains("accepted"));

    Command::cargo_bin("attest")
        .unwrap()
        .args(["pre-commit-hook", "--repo", repo.path()])
        .assert()
        .success();
}

#[test]
fn pre_commit_hook_keeps_existing_draft_for_same_staged_diff() {
    let repo = TestRepo::with_root_contract();
    repo.write("src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["pre-commit-hook", "--repo", repo.path()])
        .assert()
        .failure();

    let mut attestation = repo.read_attestation();
    attestation.items[0].evidence = vec!["partial evidence survives".to_string()];
    repo.write_attestation(&attestation);

    Command::cargo_bin("attest")
        .unwrap()
        .args(["pre-commit-hook", "--repo", repo.path()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Attestation draft is waiting at"));

    let bytes = std::fs::read_to_string(repo.attestation_path()).unwrap();
    assert!(bytes.contains("partial evidence survives"));
}

#[test]
fn pre_commit_hook_refreshes_stale_draft_when_staged_diff_changes() {
    let repo = TestRepo::with_root_contract();
    repo.write("src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["pre-commit-hook", "--repo", repo.path()])
        .assert()
        .failure();

    let mut attestation = repo.read_attestation();
    let old_digest = attestation.staged_diff_digest.clone();
    attestation.items[0].evidence = vec!["old evidence".to_string()];
    repo.write_attestation(&attestation);

    repo.write("src/lib.rs", "pub fn answer() -> u32 { 43 }\n");
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["pre-commit-hook", "--repo", repo.path()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Attestation draft written to"));

    let refreshed = repo.read_attestation();
    assert_ne!(refreshed.staged_diff_digest, old_digest);
    assert!(refreshed.items[0].evidence.is_empty());
}

#[test]
fn installed_pre_commit_hook_runs_attest() {
    let repo = TestRepo::with_root_contract();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["install-hooks", "--repo", repo.path()])
        .assert()
        .success();

    let hook = std::fs::read_to_string(repo.root.join(".git/hooks/pre-commit")).unwrap();
    assert!(hook.contains("attest pre-commit-hook"));
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
    repo.commit_all("initial inline contract");

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
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["status", "--repo", repo.path(), "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("engine.no_test_case_heuristics").not());

    repo.commit_all("change import");

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
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["review", "--repo", repo.path()])
        .assert()
        .success();

    let attestation = repo.read_attestation();
    assert_eq!(attestation.items.len(), 1);
    let item = &attestation.items[0];
    assert_eq!(
        item.contract_path.as_str(),
        "src/lib.rs#attest:engine.guarded"
    );
    assert_eq!(item.claim_id, "engine.no_test_case_heuristics");
    let target = item.target.as_ref().unwrap();
    assert_eq!(target.symbol.as_deref(), Some("guarded"));
}

#[test]
fn markdown_fenced_inline_examples_do_not_activate_contracts() {
    let repo = TestRepo::new();
    repo.write(
        "README.md",
        r#"Example:

```rust
// attest: begin
// scope: function
// id: docs.example
// module: docs
// claims:
//   - id: docs.example_reviewed
//     text: This is documentation, not a live contract.
// attest: end
pub fn example() {}
```
"#,
    );
    repo.stage_all();

    Command::cargo_bin("attest")
        .unwrap()
        .args(["status", "--repo", repo.path(), "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("docs.example_reviewed").not());
}

fn sign_all(attestation: &mut CommitAttestation) {
    attestation.signoff.signed_at = Some(Timestamp::now());
    for item in &mut attestation.items {
        item.status = Some(ClaimStatus::True);
        item.evidence = vec![format!("reviewed {}", item.claim_id)];
    }
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

    fn with_root_contract() -> Self {
        let repo = Self::new();
        repo.write(
            "AGENT_CONTRACT.yaml",
            r#"version: 1
module: repo
claims:
  - id: repo.reviewed
    text: I reviewed the repo-level impact.
"#,
        );
        repo.write("src/lib.rs", "pub fn answer() -> u32 { 41 }\n");
        repo.commit_all("initial");
        repo
    }

    fn path(&self) -> &str {
        self.root.as_str()
    }

    fn attestation_path(&self) -> Utf8PathBuf {
        self.root.join(".git/attest/pending-attestation.yaml")
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn stage_all(&self) {
        self.git(["add", "."]);
    }

    fn commit_all(&self, message: &str) {
        self.stage_all();
        self.git(["commit", "-m", message]);
    }

    fn read_attestation(&self) -> CommitAttestation {
        let bytes = std::fs::read(self.attestation_path()).unwrap();
        serde_yaml_ng::from_slice(&bytes).unwrap()
    }

    fn write_attestation(&self, attestation: &CommitAttestation) {
        std::fs::write(
            self.attestation_path(),
            serde_yaml_ng::to_string(attestation).unwrap(),
        )
        .unwrap();
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
