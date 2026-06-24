use assert_cmd::Command;
use camino::Utf8PathBuf;
use insta::{Settings, assert_snapshot};
use std::process::Command as StdCommand;
use tempfile::TempDir;

#[test]
fn status_cli_output_is_stable() {
    let repo = SnapshotRepo::with_directory_and_inline_contracts();
    let output = repo.attest_stdout(&["status", "--repo", repo.path(), "--base", "HEAD~1"]);

    snapshot_settings().bind(|| {
        assert_snapshot!("status_cli_output_is_stable", output);
    });
}

#[test]
fn review_print_cli_output_is_stable() {
    let repo = SnapshotRepo::with_directory_and_inline_contracts();
    let output = repo.attest_stdout(&[
        "review-pr",
        "--repo",
        repo.path(),
        "--base",
        "HEAD~1",
        "--print",
    ]);

    snapshot_settings().bind(|| {
        assert_snapshot!("review_print_cli_output_is_stable", output);
    });
}

#[test]
fn inline_explain_cli_output_is_stable() {
    let repo = SnapshotRepo::with_directory_and_inline_contracts();
    let output = repo.attest_stdout(&[
        "inline",
        "--repo",
        repo.path(),
        "explain",
        "crates/engine/src/lib.rs",
    ]);

    snapshot_settings().bind(|| {
        assert_snapshot!("inline_explain_cli_output_is_stable", output);
    });
}

fn snapshot_settings() -> Settings {
    let mut settings = Settings::clone_current();
    settings.add_filter(r"/[^ \n]+/\.git/attest/pr-review\.yaml", "<REVIEW_PATH>");
    settings.add_filter(
        r"at /[^;\n]+/\.git/attest/pr-attestation\.json",
        "at <ATTESTATION_PATH>",
    );
    settings.add_filter(r"\b[0-9a-f]{40}\b", "<SHA>");
    settings.add_filter(r"\b[0-9a-f]{8}\b", "<SHORT_SHA>");
    settings.add_filter(r"blake3:[0-9a-f]{64}", "blake3:<DIGEST>");
    settings.add_filter(r"generated_at: .+", "generated_at: <TIMESTAMP>");
    settings.add_filter(r"signed_at: .+", "signed_at: <TIMESTAMP>");
    settings
}

struct SnapshotRepo {
    _tmp: TempDir,
    root: Utf8PathBuf,
}

impl SnapshotRepo {
    fn with_directory_and_inline_contracts() -> Self {
        let repo = Self::new();
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
"#,
        );
        repo.commit_all("initial");

        repo.write(
            "crates/engine/src/lib.rs",
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
"#,
        );
        repo.commit_all("change guarded");
        repo
    }

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

    fn commit_all(&self, message: &str) {
        self.git(["add", "."]);
        self.git(["commit", "-m", message]);
    }

    fn attest_stdout(&self, args: &[&str]) -> String {
        let output = Command::cargo_bin("attest")
            .unwrap()
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "attest failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
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
