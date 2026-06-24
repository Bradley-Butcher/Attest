use assert_cmd::Command;
use attest_contracts::{AttestationItem, CommitAttestation, ContractTargetKind};
use camino::Utf8PathBuf;
use std::process::Command as StdCommand;
use tempfile::TempDir;

#[test]
fn parent_folder_contract_applies_to_descendant_staged_change() {
    let repo = TestRepo::new();
    repo.write(
        "AGENT_CONTRACT.yaml",
        r#"version: 1
module: repo
claims:
  - id: repo.parent_reviewed
    text: I reviewed the repository-level impact.
"#,
    );
    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 41 }\n",
    );
    repo.commit_all("initial");

    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n",
    );
    repo.stage_all();

    let attestation = repo.review();
    let item = only_attestation_item(&attestation);
    assert_eq!(item.contract_path.as_str(), "AGENT_CONTRACT.yaml");
    assert_eq!(item.claim_id, "repo.parent_reviewed");
    assert_eq!(item.changed_files, vec!["crates/engine/src/lib.rs"]);
}

#[test]
fn sub_folder_contract_applies_to_descendant_staged_change() {
    let repo = TestRepo::new();
    repo.write(
        "crates/engine/AGENT_CONTRACT.yaml",
        r#"version: 1
module: engine
claims:
  - id: engine.subfolder_reviewed
    text: I reviewed the engine-level impact.
"#,
    );
    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 41 }\n",
    );
    repo.commit_all("initial");

    repo.write(
        "crates/engine/src/lib.rs",
        "pub fn answer() -> u32 { 42 }\n",
    );
    repo.stage_all();

    let attestation = repo.review();
    let item = only_attestation_item(&attestation);
    assert_eq!(
        item.contract_path.as_str(),
        "crates/engine/AGENT_CONTRACT.yaml"
    );
    assert_eq!(item.claim_id, "engine.subfolder_reviewed");
    assert_eq!(item.changed_files, vec!["crates/engine/src/lib.rs"]);
}

#[test]
fn inline_script_contract_applies_to_script_staged_change() {
    let repo = TestRepo::new();
    repo.write(
        "scripts/reconcile.py",
        r#"# attest: begin
# scope: script
# id: scripts.reconcile
# module: scripts
# claims:
#   - id: scripts.reconcile_safe_by_default
#     text: reconcile.py is safe to run without mutating production data.
# attest: end

def main():
    print("dry-run")

if __name__ == "__main__":
    main()
"#,
    );
    repo.commit_all("initial script");

    repo.write(
        "scripts/reconcile.py",
        r#"# attest: begin
# scope: script
# id: scripts.reconcile
# module: scripts
# claims:
#   - id: scripts.reconcile_safe_by_default
#     text: reconcile.py is safe to run without mutating production data.
# attest: end

def main():
    print("dry-run v2")

if __name__ == "__main__":
    main()
"#,
    );
    repo.stage_all();

    let attestation = repo.review();
    let item = only_attestation_item(&attestation);
    assert_eq!(
        item.contract_path.as_str(),
        "scripts/reconcile.py#attest:scripts.reconcile"
    );
    assert_eq!(item.claim_id, "scripts.reconcile_safe_by_default");
    let target = item.target.as_ref().expect("script target");
    assert_eq!(target.kind, ContractTargetKind::Script);
    assert_eq!(target.path, "scripts/reconcile.py");
}

#[test]
fn inline_file_contract_applies_to_file_staged_change() {
    let repo = TestRepo::new();
    repo.write(
        "docs/release.md",
        r#"# attest: begin
# scope: file
# id: docs.release_notes
# module: docs
# claims:
#   - id: docs.release_notes_reviewed
#     text: Release notes accurately describe user-visible behavior.
# attest: end

Initial release notes.
"#,
    );
    repo.commit_all("initial docs");

    repo.write(
        "docs/release.md",
        r#"# attest: begin
# scope: file
# id: docs.release_notes
# module: docs
# claims:
#   - id: docs.release_notes_reviewed
#     text: Release notes accurately describe user-visible behavior.
# attest: end

Updated release notes.
"#,
    );
    repo.stage_all();

    let attestation = repo.review();
    let item = only_attestation_item(&attestation);
    assert_eq!(
        item.contract_path.as_str(),
        "docs/release.md#attest:docs.release_notes"
    );
    assert_eq!(item.claim_id, "docs.release_notes_reviewed");
    let target = item.target.as_ref().expect("file target");
    assert_eq!(target.kind, ContractTargetKind::File);
    assert_eq!(target.path, "docs/release.md");
}

#[test]
fn inline_function_contract_applies_to_function_staged_change() {
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
    repo.commit_all("initial function");

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

    let attestation = repo.review();
    let item = only_attestation_item(&attestation);
    assert_eq!(
        item.contract_path.as_str(),
        "src/lib.rs#attest:engine.guarded"
    );
    assert_eq!(item.claim_id, "engine.no_test_case_heuristics");
    let target = item.target.as_ref().expect("function target");
    assert_eq!(target.kind, ContractTargetKind::Function);
    assert_eq!(target.path, "src/lib.rs");
    assert_eq!(target.symbol.as_deref(), Some("guarded"));
}

fn only_attestation_item(attestation: &CommitAttestation) -> &AttestationItem {
    assert_eq!(attestation.items.len(), 1, "expected one active claim");
    &attestation.items[0]
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

    fn stage_all(&self) {
        self.git(["add", "."]);
    }

    fn commit_all(&self, message: &str) {
        self.stage_all();
        self.git(["commit", "-m", message]);
    }

    fn review(&self) -> CommitAttestation {
        Command::cargo_bin("attest")
            .unwrap()
            .args(["review", "--repo", self.path()])
            .assert()
            .success();
        self.read_attestation()
    }

    fn read_attestation(&self) -> CommitAttestation {
        let path = self.root.join(".git/attest/pending-attestation.yaml");
        let bytes = std::fs::read(path).unwrap();
        serde_yaml_ng::from_slice(&bytes).unwrap()
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
