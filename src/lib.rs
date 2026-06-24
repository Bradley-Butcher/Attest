use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand, ValueEnum};
use git2::{DiffFormat, DiffOptions, ObjectType, Oid, Repository, StatusOptions, Tree};
use jiff::Timestamp;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tree_sitter::{Language, Node, Parser as TsParser};

const CONTRACT_FILENAMES: &[&str] = &[
    "AGENT_CONTRACT.yaml",
    "AGENT_CONTRACT.yml",
    "attest.yaml",
    "attest.yml",
];

const ATTEST_DIR: &str = "attest";
const ATTESTATION_FILE: &str = "pending-attestation.yaml";
const INLINE_CACHE_VERSION: &str = "attest.inline-cache.v1";
const INLINE_BEGIN: &[u8] = b"attest: begin";
const COMMIT_ATTESTATION_KIND: &str = "attest.commit.v1";

#[derive(Debug, Parser)]
#[command(
    name = "attest",
    version,
    about = "Staged commit contract attestations for coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a starter AGENT_CONTRACT.yaml.
    Init(InitArgs),
    /// Show required contracts for the staged commit.
    Status(StatusArgs),
    /// Create or refresh the pending commit attestation draft.
    Review(ReviewArgs),
    /// Verify the pending commit attestation draft.
    Verify(VerifyArgs),
    /// Git pre-commit hook entrypoint.
    PreCommitHook(HookArgs),
    /// Install the Git pre-commit hook for this repository.
    InstallHooks(InstallHookArgs),
    /// Inspect inline comment contracts in source files.
    Inline(InlineArgs),
    /// Emit JSON schema for contract or attestation files.
    Schema(SchemaArgs),
}

#[derive(Debug, Args, Clone)]
struct RepoArgs {
    /// Repository path. Defaults to the current directory.
    #[arg(long)]
    repo: Option<Utf8PathBuf>,
}

#[derive(Debug, Args)]
struct InitArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Overwrite an existing contract file.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReviewArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Attestation draft path. Defaults to .git/attest/pending-attestation.yaml.
    #[arg(long)]
    output: Option<Utf8PathBuf>,

    /// Overwrite an existing draft even when it matches the staged diff.
    #[arg(long)]
    force: bool,

    /// Also print the generated attestation draft.
    #[arg(long)]
    print: bool,

    /// Agent kind recorded in the draft.
    #[arg(long, default_value = "codex", env = "ATTEST_AGENT_KIND")]
    agent_kind: String,

    /// Agent/session identifier recorded in the draft.
    #[arg(long, env = "ATTEST_AGENT_SESSION")]
    session_id: Option<String>,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Attestation draft path. Defaults to .git/attest/pending-attestation.yaml.
    #[arg(long)]
    attestation: Option<Utf8PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HookArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Agent kind recorded when the hook creates a draft.
    #[arg(long, default_value = "codex", env = "ATTEST_AGENT_KIND")]
    agent_kind: String,

    /// Agent/session identifier recorded when the hook creates a draft.
    #[arg(long, env = "ATTEST_AGENT_SESSION")]
    session_id: Option<String>,
}

#[derive(Debug, Args)]
struct InstallHookArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Overwrite an existing pre-commit hook.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct InlineArgs {
    #[command(flatten)]
    repo: RepoArgs,

    #[command(subcommand)]
    command: InlineCommand,
}

#[derive(Debug, Subcommand)]
enum InlineCommand {
    /// Validate inline contracts in a source file.
    Check(InlinePathArgs),
    /// Show which target each inline contract binds to.
    Explain(InlinePathArgs),
}

#[derive(Debug, Args)]
struct InlinePathArgs {
    /// Source file to inspect, relative to the repository root.
    path: Utf8PathBuf,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SchemaArgs {
    #[arg(value_enum)]
    kind: SchemaKind,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SchemaKind {
    Contract,
    Attestation,
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init(args) => init(args),
        Command::Status(args) => status(args),
        Command::Review(args) => review(args),
        Command::Verify(args) => verify(args),
        Command::PreCommitHook(args) => pre_commit_hook(args),
        Command::InstallHooks(args) => install_hooks(args),
        Command::Inline(args) => inline(args),
        Command::Schema(args) => schema(args),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Contract {
    #[serde(default = "default_contract_version")]
    pub version: u32,
    pub module: String,
    #[serde(default)]
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub required_checks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Claim {
    pub id: String,
    pub text: String,
    #[serde(default, alias = "challenge", alias = "review_questions")]
    pub review: Vec<String>,
    #[serde(default = "default_true")]
    pub evidence_required: bool,
}

#[derive(Debug, Clone)]
struct LoadedContract {
    path: Utf8PathBuf,
    scope: String,
    digest: String,
    contract: Contract,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActiveContract {
    pub module: String,
    #[schemars(with = "String")]
    pub path: Utf8PathBuf,
    #[serde(default)]
    pub source: ContractSource,
    pub scope: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ContractTarget>,
    pub changed_files: Vec<String>,
    pub claims: Vec<Claim>,
    pub required_checks: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContractSource {
    #[default]
    Directory,
    Inline,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ContractTarget {
    pub kind: ContractTargetKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub contract_start_line: usize,
    pub contract_end_line: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContractTargetKind {
    File,
    Script,
    Function,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommitState {
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    pub staged_tree: String,
    pub staged_diff_digest: String,
    pub changed_files: Vec<String>,
    pub unstaged_files: Vec<String>,
    pub active_contracts: Vec<ActiveContract>,
    #[schemars(with = "String")]
    pub attestation_path: Utf8PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommitAttestation {
    pub kind: String,
    pub generated_at: String,
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    pub staged_tree: String,
    pub staged_diff_digest: String,
    pub signoff: AttestationSignoff,
    pub items: Vec<AttestationItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttestationSignoff {
    #[serde(default = "default_agent_kind")]
    pub agent_kind: String,
    #[serde(default)]
    pub agent_session: Option<String>,
    #[serde(default)]
    pub signed_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttestationItem {
    #[schemars(with = "String")]
    pub contract_path: Utf8PathBuf,
    pub contract_digest: String,
    pub module: String,
    #[serde(default)]
    pub source: ContractSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ContractTarget>,
    pub claim_id: String,
    pub claim: String,
    #[serde(default)]
    pub review_questions: Vec<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub evidence_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ClaimStatus>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    #[serde(rename = "true")]
    True,
    #[serde(rename = "false")]
    False,
    Unsure,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VerifyReport {
    pub verdict: Verdict,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub required_contracts: Vec<ActiveContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AttestationSummary>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accepted,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttestationSummary {
    #[schemars(with = "String")]
    pub path: Utf8PathBuf,
    pub signed_at: Timestamp,
    pub agent_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<String>,
}

struct RepoContext {
    repo: Repository,
    root: Utf8PathBuf,
    git_dir: Utf8PathBuf,
}

impl RepoContext {
    fn open(repo_path: &Option<Utf8PathBuf>) -> Result<Self> {
        let start = repo_path
            .as_deref()
            .map(Utf8Path::as_std_path)
            .unwrap_or_else(|| Path::new("."));
        let repo = Repository::discover(start).context("failed to discover a Git repository")?;
        let workdir = repo
            .workdir()
            .context("Attest requires a non-bare Git repository")?;
        let root = Utf8PathBuf::from_path_buf(workdir.to_path_buf())
            .map_err(|path| anyhow::anyhow!("repository path is not UTF-8: {}", path.display()))?;
        let git_dir = Utf8PathBuf::from_path_buf(repo.path().to_path_buf()).map_err(|path| {
            anyhow::anyhow!("git directory path is not UTF-8: {}", path.display())
        })?;
        Ok(Self {
            repo,
            root,
            git_dir,
        })
    }

    fn attest_dir(&self) -> Utf8PathBuf {
        self.git_dir.join(ATTEST_DIR)
    }

    fn attestation_path(&self) -> Utf8PathBuf {
        self.attest_dir().join(ATTESTATION_FILE)
    }

    fn inline_cache_dir(&self) -> Utf8PathBuf {
        self.attest_dir().join("cache").join("blobs")
    }

    fn ensure_attest_dir(&self) -> Result<()> {
        fs_err::create_dir_all(self.attest_dir()).context("failed to create .git/attest")
    }

    fn ensure_inline_cache_dir(&self) -> Result<()> {
        fs_err::create_dir_all(self.inline_cache_dir())
            .context("failed to create .git/attest/cache/blobs")
    }
}

fn init(args: InitArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.repo.repo)?;
    let path = ctx.root.join("AGENT_CONTRACT.yaml");
    if path.exists() && !args.force {
        bail!(
            "{} already exists. Re-run with --force to overwrite it.",
            path
        );
    }

    fs_err::write(&path, sample_contract()).with_context(|| format!("failed to write {path}"))?;
    println!("Created {path}");
    Ok(())
}

fn sample_contract() -> &'static str {
    r#"version: 1
module: repo
claims:
  - id: repo.abstraction_boundary_preserved
    text: I preserved the abstraction boundaries this change touches.
    review:
      - Name each boundary touched by the staged diff.
      - Explain why the behavior belongs behind that boundary.
      - Call out any shortcut that would make the code harder to reason about later.
  - id: repo.product_expectation_preserved
    text: I preserved the product expectation this code is responsible for.
    review:
      - State the product expectation in one sentence.
      - Explain how the staged diff keeps that expectation true.
"#
}

fn status(args: StatusArgs) -> Result<()> {
    let (ctx, state) = commit_state_from_args(&args.repo)?;
    let report = verify_state(&ctx, &state, None)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_state(&state);
    print_report(&report);
    Ok(())
}

fn review(args: ReviewArgs) -> Result<()> {
    let (ctx, state) = commit_state_from_args(&args.repo)?;
    let output = args.output.unwrap_or_else(|| ctx.attestation_path());
    let action = ensure_draft_for_state(
        &ctx,
        &state,
        &output,
        &args.agent_kind,
        args.session_id.clone(),
        args.force,
    )?;

    match action {
        DraftAction::Created => println!("Wrote attestation draft to {output}"),
        DraftAction::Replaced => println!("Refreshed stale attestation draft at {output}"),
        DraftAction::Kept => println!("Kept current attestation draft at {output}"),
    }
    if state.active_contracts.is_empty() {
        println!("No contracts apply to the staged commit.");
    } else {
        println!(
            "Review {} claim(s), set every status to true, add evidence, set signed_at, then run `git commit` again.",
            state
                .active_contracts
                .iter()
                .map(|contract| contract.claims.len())
                .sum::<usize>()
        );
    }
    if args.print {
        let bytes =
            fs_err::read(&output).with_context(|| format!("failed to read draft {output}"))?;
        println!("\n{}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

fn verify(args: VerifyArgs) -> Result<()> {
    let (ctx, state) = commit_state_from_args(&args.repo)?;
    let report = verify_state(&ctx, &state, args.attestation.as_ref())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if report.verdict == Verdict::Blocked {
        bail!("commit attestation verification failed");
    }
    Ok(())
}

fn pre_commit_hook(args: HookArgs) -> Result<()> {
    let (ctx, state) = commit_state_from_args(&args.repo)?;
    if state.active_contracts.is_empty() {
        return Ok(());
    }

    let path = ctx.attestation_path();
    let action = ensure_draft_for_state(
        &ctx,
        &state,
        &path,
        &args.agent_kind,
        args.session_id.clone(),
        false,
    )?;
    let report = verify_state(&ctx, &state, Some(&path))?;
    if report.verdict == Verdict::Accepted {
        return Ok(());
    }

    eprintln!("{}", pre_commit_block_message(&ctx, &state, &path, action));
    std::process::exit(1);
}

fn install_hooks(args: InstallHookArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.repo.repo)?;
    let hook_path = write_pre_commit_hook(&ctx, args.force)?;
    println!("Installed Git pre-commit hook at {hook_path}");
    Ok(())
}

fn inline(args: InlineArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.repo.repo)?;
    match args.command {
        InlineCommand::Check(path_args) => inline_check_or_explain(&ctx, path_args, false),
        InlineCommand::Explain(path_args) => inline_check_or_explain(&ctx, path_args, true),
    }
}

fn inline_check_or_explain(ctx: &RepoContext, args: InlinePathArgs, explain: bool) -> Result<()> {
    let relative = repo_relative_path(ctx, &args.path)?;
    let contracts = inline_contracts_from_worktree(ctx, &relative)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&contracts)?);
        return Ok(());
    }

    if contracts.is_empty() {
        println!("{relative}: no inline contracts");
        return Ok(());
    }

    if explain {
        println!("{relative}: {} inline contract(s)", contracts.len());
        for contract in contracts {
            let target = match contract.scope.target_kind() {
                ContractTargetKind::Function => contract
                    .symbol
                    .as_deref()
                    .map(|symbol| format!("function {symbol}"))
                    .unwrap_or_else(|| "function <unknown>".to_string()),
                ContractTargetKind::Script => "script".to_string(),
                ContractTargetKind::File => "file".to_string(),
            };
            println!("  - {}", contract.id);
            println!(
                "      target: {target}, lines {}-{}",
                contract.target_span.start_line, contract.target_span.end_line
            );
            println!(
                "      contract: lines {}-{}",
                contract.comment_span.start_line, contract.comment_span.end_line
            );
        }
    } else {
        println!("{relative}: {} inline contract(s) valid", contracts.len());
    }
    Ok(())
}

fn inline_contracts_from_worktree(
    ctx: &RepoContext,
    relative: &Utf8Path,
) -> Result<Vec<CachedInlineContract>> {
    let full_path = ctx.root.join(relative);
    let bytes = fs_err::read(&full_path).with_context(|| format!("failed to read {full_path}"))?;
    parse_inline_contract_blob(
        relative.as_str(),
        &bytes,
        language_for_path(relative.as_str()),
    )
}

fn repo_relative_path(ctx: &RepoContext, path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        return path
            .strip_prefix(&ctx.root)
            .map(Utf8Path::to_path_buf)
            .with_context(|| format!("{path} is not inside {}", ctx.root));
    }
    Ok(path.to_path_buf())
}

fn write_pre_commit_hook(ctx: &RepoContext, force: bool) -> Result<Utf8PathBuf> {
    let hook_path = ctx.git_dir.join("hooks").join("pre-commit");
    if hook_path.exists() && !force {
        bail!("{hook_path} already exists. Re-run with --force to overwrite it.");
    }
    let hook = "#!/bin/sh\nset -eu\nattest pre-commit-hook\n";
    fs_err::write(&hook_path, hook).with_context(|| format!("failed to write {hook_path}"))?;
    make_executable(&hook_path)?;
    Ok(hook_path)
}

fn schema(args: SchemaArgs) -> Result<()> {
    let value = match args.kind {
        SchemaKind::Contract => serde_json::to_value(schema_for!(Contract))?,
        SchemaKind::Attestation => serde_json::to_value(schema_for!(CommitAttestation))?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn commit_state_from_args(args: &RepoArgs) -> Result<(RepoContext, CommitState)> {
    let ctx = RepoContext::open(&args.repo)?;
    let state = build_commit_state(&ctx)?;
    Ok((ctx, state))
}

// attest: begin
// scope: function
// id: attest_core.build_commit_state
// module: attest-core
// claims:
//   - id: attest_core.build_commit_state_preserves_commit_mental_model
//     text: build_commit_state preserves the user's mental model that Attest is reviewing the staged commit, not a PR, branch, or working tree mixture.
//     review:
//       - Explain the product meaning of HEAD, the index tree, changed files, and unstaged files in this change.
//       - Identify any state that is observed but intentionally not signed.
//       - Call out any behavior that would surprise a user comparing `git diff --cached` to Attest output.
//   - id: attest_core.build_commit_state_keeps_activation_explainable
//     text: build_commit_state keeps contract activation explainable from staged paths and staged line changes rather than hidden repository state.
//     review:
//       - Explain why each newly active contract would make sense to a maintainer looking at the staged diff.
//       - Identify any activation that depends on unrelated files, branch names, or remotes.
//       - Call out any behavior that feels conservative enough for pre-commit but too noisy for routine use.
// attest: end
fn build_commit_state(ctx: &RepoContext) -> Result<CommitState> {
    let mut index = ctx.repo.index().context("failed to read Git index")?;
    if index.has_conflicts() {
        bail!("Attest cannot sign a commit while the Git index has unresolved conflicts.");
    }
    let staged_tree_oid = index
        .write_tree()
        .context("failed to write staged index tree")?;
    let staged_tree = ctx
        .repo
        .find_tree(staged_tree_oid)
        .context("failed to read staged index tree")?;
    let (base_tree_oid, base_sha) = head_tree(ctx)?;
    let base_tree = ctx
        .repo
        .find_tree(base_tree_oid)
        .context("failed to read HEAD tree")?;

    let diff = diff_between_trees(&ctx.repo, &base_tree, &staged_tree)?;
    let file_changes = collect_file_changes(&diff)?;
    let changed_files = changed_files_from_changes(&file_changes);
    let staged_diff_digest = diff_digest(&diff, base_tree_oid, staged_tree_oid, &changed_files)?;

    let contracts =
        load_directory_contracts_for_paths(ctx, &changed_files, &base_tree, &staged_tree)?;
    let mut active_contracts = Vec::new();
    for loaded in contracts {
        let matched: Vec<String> = changed_files
            .iter()
            .filter(|path| path_is_within_scope(path, &loaded.scope))
            .cloned()
            .collect();
        if matched.is_empty() {
            continue;
        }
        active_contracts.push(ActiveContract {
            module: loaded.contract.module.clone(),
            path: loaded.path,
            source: ContractSource::Directory,
            scope: loaded.scope,
            digest: loaded.digest,
            target: None,
            changed_files: matched,
            claims: loaded.contract.claims,
            required_checks: loaded.contract.required_checks,
        });
    }

    active_contracts.extend(load_active_inline_contracts(ctx, &file_changes)?);
    active_contracts.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(CommitState {
        base: "HEAD".to_string(),
        base_sha,
        staged_tree: staged_tree_oid.to_string(),
        staged_diff_digest,
        changed_files,
        unstaged_files: unstaged_files(ctx)?,
        active_contracts,
        attestation_path: ctx.attestation_path(),
    })
}

fn head_tree(ctx: &RepoContext) -> Result<(Oid, Option<String>)> {
    match ctx.repo.head().and_then(|head| head.peel_to_commit()) {
        Ok(commit) => Ok((commit.tree_id(), Some(commit.id().to_string()))),
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) =>
        {
            Ok((empty_tree_oid(&ctx.repo)?, None))
        }
        Err(error) => Err(error).context("failed to resolve HEAD"),
    }
}

fn empty_tree_oid(repo: &Repository) -> Result<Oid> {
    let builder = repo.treebuilder(None)?;
    builder.write().context("failed to create empty tree")
}

fn diff_between_trees<'repo>(
    repo: &'repo Repository,
    old_tree: &Tree<'repo>,
    new_tree: &Tree<'repo>,
) -> Result<git2::Diff<'repo>> {
    let mut options = DiffOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    repo.diff_tree_to_tree(Some(old_tree), Some(new_tree), Some(&mut options))
        .context("failed to compute staged diff")
}

#[derive(Debug, Clone)]
struct FileChange {
    old_path: Option<String>,
    new_path: Option<String>,
    old_oid: Option<Oid>,
    new_oid: Option<Oid>,
    old_changed_lines: Vec<LineRange>,
    new_changed_lines: Vec<LineRange>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct LineRange {
    pub start_line: usize,
    pub end_line: usize,
}

impl LineRange {
    fn one(line: u32) -> Self {
        let line = line as usize;
        Self {
            start_line: line,
            end_line: line,
        }
    }

    fn intersects(&self, other: &Self) -> bool {
        self.start_line <= other.end_line && other.start_line <= self.end_line
    }
}

fn collect_file_changes(diff: &git2::Diff<'_>) -> Result<Vec<FileChange>> {
    let mut changes = BTreeMap::<String, FileChange>::new();
    for delta in diff.deltas() {
        changes
            .entry(delta_key(&delta))
            .or_insert_with(|| file_change_from_delta(&delta));
    }
    diff.foreach(
        &mut |_delta, _progress| true,
        None,
        None,
        Some(&mut |delta, _hunk, line| {
            let key = delta_key(&delta);
            let change = changes
                .entry(key)
                .or_insert_with(|| file_change_from_delta(&delta));
            match line.origin() {
                '-' => {
                    if let Some(line_number) = line.old_lineno() {
                        change.old_changed_lines.push(LineRange::one(line_number));
                    }
                }
                '+' => {
                    if let Some(line_number) = line.new_lineno() {
                        change.new_changed_lines.push(LineRange::one(line_number));
                    }
                }
                _ => {}
            }
            true
        }),
    )?;

    let mut changes = changes.into_values().collect::<Vec<_>>();
    for change in &mut changes {
        compact_ranges(&mut change.old_changed_lines);
        compact_ranges(&mut change.new_changed_lines);
    }
    Ok(changes)
}

fn file_change_from_delta(delta: &git2::DiffDelta<'_>) -> FileChange {
    let old_oid = nonzero_oid(delta.old_file().id());
    let new_oid = nonzero_oid(delta.new_file().id());
    FileChange {
        old_path: delta.old_file().path().and_then(normalize_std_path),
        new_path: delta.new_file().path().and_then(normalize_std_path),
        old_oid,
        new_oid,
        old_changed_lines: Vec::new(),
        new_changed_lines: Vec::new(),
    }
}

fn delta_key(delta: &git2::DiffDelta<'_>) -> String {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .and_then(normalize_std_path)
        .unwrap_or_else(|| delta.new_file().id().to_string())
}

fn nonzero_oid(oid: Oid) -> Option<Oid> {
    (!oid.is_zero()).then_some(oid)
}

fn compact_ranges(ranges: &mut Vec<LineRange>) {
    ranges.sort_by_key(|range| (range.start_line, range.end_line));
    let mut compacted: Vec<LineRange> = Vec::new();
    for range in ranges.drain(..) {
        if let Some(last) = compacted.last_mut()
            && range.start_line <= last.end_line + 1
        {
            last.end_line = last.end_line.max(range.end_line);
            continue;
        }
        compacted.push(range);
    }
    *ranges = compacted;
}

fn changed_files_from_changes(changes: &[FileChange]) -> Vec<String> {
    let mut files = BTreeSet::new();
    for change in changes {
        if let Some(path) = &change.old_path {
            files.insert(path.clone());
        }
        if let Some(path) = &change.new_path {
            files.insert(path.clone());
        }
    }
    files.into_iter().collect()
}

fn diff_digest(
    diff: &git2::Diff<'_>,
    base_tree: Oid,
    staged_tree: Oid,
    changed_files: &[String],
) -> Result<String> {
    let mut patch = Vec::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        patch.push(line.origin() as u8);
        patch.extend_from_slice(line.content());
        true
    })?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"attest.staged-diff.v1\n");
    hasher.update(base_tree.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(staged_tree.to_string().as_bytes());
    hasher.update(b"\n");
    for path in changed_files {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(&patch);
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn unstaged_files(ctx: &RepoContext) -> Result<Vec<String>> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let statuses = ctx.repo.statuses(Some(&mut options))?;
    let mut paths = BTreeSet::new();
    for entry in statuses.iter() {
        let status = entry.status();
        if (status.is_wt_new()
            || status.is_wt_modified()
            || status.is_wt_deleted()
            || status.is_wt_renamed()
            || status.is_wt_typechange())
            && let Ok(path) = entry.path()
        {
            paths.insert(path.replace('\\', "/"));
        }
    }
    Ok(paths.into_iter().collect())
}

fn load_directory_contracts_for_paths(
    ctx: &RepoContext,
    changed_files: &[String],
    base_tree: &Tree<'_>,
    staged_tree: &Tree<'_>,
) -> Result<Vec<LoadedContract>> {
    let mut contracts = Vec::new();
    let mut seen = BTreeSet::new();
    for relative in candidate_contract_paths(changed_files) {
        if !seen.insert(relative.clone()) {
            continue;
        }
        let staged_bytes = blob_bytes_from_tree(&ctx.repo, staged_tree, &relative)?;
        let base_bytes = blob_bytes_from_tree(&ctx.repo, base_tree, &relative)?;
        let Some(bytes) = staged_bytes.or(base_bytes) else {
            continue;
        };
        let contract: Contract = serde_yaml_ng::from_slice(&bytes)
            .with_context(|| format!("failed to parse contract {relative}"))?;
        validate_contract(&relative, &contract)?;
        contracts.push(LoadedContract {
            scope: scope_for_contract(&relative),
            path: relative,
            digest: digest_bytes(&bytes),
            contract,
        });
    }
    Ok(contracts)
}

fn blob_bytes_from_tree(
    repo: &Repository,
    tree: &Tree<'_>,
    path: &Utf8Path,
) -> Result<Option<Vec<u8>>> {
    let entry = match tree.get_path(path.as_std_path()) {
        Ok(entry) => entry,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {path} from tree"));
        }
    };
    if entry.kind() != Some(ObjectType::Blob) {
        return Ok(None);
    }
    let blob = repo
        .find_blob(entry.id())
        .with_context(|| format!("failed to read blob for {path}"))?;
    Ok(Some(blob.content().to_vec()))
}

fn candidate_contract_paths(changed_files: &[String]) -> Vec<Utf8PathBuf> {
    let mut paths = BTreeSet::new();
    for changed_file in changed_files {
        let path = Utf8Path::new(changed_file);
        let mut directory = path.parent();
        loop {
            let prefix = directory.unwrap_or_else(|| Utf8Path::new(""));
            for name in CONTRACT_FILENAMES {
                paths.insert(prefix.join(name));
            }
            let Some(current) = directory else {
                break;
            };
            directory = current.parent();
        }
    }
    paths.into_iter().collect()
}

fn validate_contract(path: &Utf8Path, contract: &Contract) -> Result<()> {
    if contract.module.trim().is_empty() {
        bail!("{path}: module must not be empty");
    }
    if contract.claims.is_empty() {
        bail!("{path}: contract must contain at least one claim");
    }
    let mut ids = BTreeSet::new();
    for claim in &contract.claims {
        if claim.id.trim().is_empty() {
            bail!("{path}: claim id must not be empty");
        }
        if !ids.insert(&claim.id) {
            bail!("{path}: duplicate claim id `{}`", claim.id);
        }
        if claim.text.trim().is_empty() {
            bail!("{path}: claim `{}` text must not be empty", claim.id);
        }
    }
    Ok(())
}

fn scope_for_contract(contract_path: &Utf8Path) -> String {
    let parent = contract_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    if parent.as_str().is_empty() {
        String::new()
    } else {
        parent.as_str().trim_end_matches('/').to_string()
    }
}

fn path_is_within_scope(path: &str, scope: &str) -> bool {
    scope.is_empty() || path == scope || path.starts_with(&format!("{scope}/"))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum InlineScope {
    File,
    Script,
    Function,
}

impl InlineScope {
    fn target_kind(self) -> ContractTargetKind {
        match self {
            InlineScope::File => ContractTargetKind::File,
            InlineScope::Script => ContractTargetKind::Script,
            InlineScope::Function => ContractTargetKind::Function,
        }
    }
}

#[derive(Debug, Deserialize)]
struct InlineContractYaml {
    scope: Option<InlineScope>,
    id: String,
    module: Option<String>,
    #[serde(default)]
    claims: Vec<Claim>,
    #[serde(default)]
    required_checks: Vec<String>,
}

#[derive(Debug, Clone)]
struct InlineBlock {
    yaml: String,
    span: CachedSpan,
    end_byte: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InlineCacheEntry {
    kind: String,
    blob_sha: String,
    language: String,
    contracts: Vec<CachedInlineContract>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedInlineContract {
    id: String,
    module: String,
    scope: InlineScope,
    digest: String,
    comment_span: CachedSpan,
    target_span: CachedSpan,
    symbol: Option<String>,
    claims: Vec<Claim>,
    required_checks: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CachedSpan {
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
}

impl CachedSpan {
    fn line_range(&self) -> LineRange {
        LineRange {
            start_line: self.start_line,
            end_line: self.end_line,
        }
    }
}

#[derive(Debug, Clone)]
struct LanguageSpec {
    name: &'static str,
    language: Language,
    function_kinds: &'static [&'static str],
}

#[derive(Debug, Clone)]
struct FunctionCandidate {
    span: CachedSpan,
    symbol: Option<String>,
}

// attest: begin
// scope: function
// id: attest_core.load_active_inline_contracts
// module: attest-core
// claims:
//   - id: attest_core.inline_matching_respects_code_movement
//     text: Inline contract activation treats code movement, contract movement, additions, and deletions as design events that deserve review when they affect ownership.
//     review:
//       - Explain what ownership or product expectation changes when a contract appears, disappears, or moves.
//       - Identify any case where preserving old evidence would feel misleading.
//       - Call out any movement case where review would be noisy rather than useful.
//   - id: attest_core.inline_matching_respects_locality
//     text: Inline contract activation respects locality so nearby unrelated edits do not turn precise function contracts into file-wide bureaucracy.
//     review:
//       - Explain why the chosen span is the right review boundary for the contract.
//       - Identify any unrelated edit that now triggers a function contract.
//       - Call out any case where a file-level contract would better express the product expectation.
// attest: end
fn load_active_inline_contracts(
    ctx: &RepoContext,
    file_changes: &[FileChange],
) -> Result<Vec<ActiveContract>> {
    let mut active = Vec::new();
    for change in file_changes {
        let old_contracts =
            read_inline_contracts_for_side(ctx, change.old_path.as_deref(), change.old_oid)?;
        let new_contracts =
            read_inline_contracts_for_side(ctx, change.new_path.as_deref(), change.new_oid)?;
        if old_contracts.is_empty() && new_contracts.is_empty() {
            continue;
        }

        let old_by_id = inline_contracts_by_id(change.old_path.as_deref(), old_contracts)?;
        let new_by_id = inline_contracts_by_id(change.new_path.as_deref(), new_contracts)?;
        let ids = old_by_id
            .keys()
            .chain(new_by_id.keys())
            .cloned()
            .collect::<BTreeSet<_>>();

        for id in ids {
            let old = old_by_id.get(&id);
            let new = new_by_id.get(&id);
            if !inline_contract_needs_attestation(change, old, new) {
                continue;
            }
            let Some(contract) = new.or(old) else {
                continue;
            };
            let file_path = change
                .new_path
                .as_deref()
                .or(change.old_path.as_deref())
                .unwrap_or("<unknown>");
            active.push(active_inline_contract(file_path, contract));
        }
    }
    Ok(active)
}

fn read_inline_contracts_for_side(
    ctx: &RepoContext,
    path: Option<&str>,
    oid: Option<Oid>,
) -> Result<Vec<CachedInlineContract>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if !inline_contracts_supported_for_path(path) {
        return Ok(Vec::new());
    }
    let Some(oid) = oid else {
        return Ok(Vec::new());
    };
    let blob = ctx
        .repo
        .find_blob(oid)
        .with_context(|| format!("failed to read blob {oid} for {path}"))?;
    let bytes = blob.content();
    if !contains_bytes(bytes, INLINE_BEGIN) {
        return Ok(Vec::new());
    }

    let language = language_for_path(path);
    let language_name = language
        .as_ref()
        .map(|language| language.name)
        .unwrap_or("plain");
    let cache_path = inline_cache_path(ctx, oid, language_name);
    if let Ok(bytes) = fs_err::read(&cache_path)
        && let Ok(entry) = serde_json::from_slice::<InlineCacheEntry>(&bytes)
        && entry.kind == INLINE_CACHE_VERSION
        && entry.blob_sha == oid.to_string()
        && entry.language == language_name
    {
        return Ok(entry.contracts);
    }

    let contracts = parse_inline_contract_blob(path, bytes, language)
        .with_context(|| format!("failed to parse inline contracts in {path}"))?;
    if ctx.ensure_inline_cache_dir().is_ok() {
        let entry = InlineCacheEntry {
            kind: INLINE_CACHE_VERSION.to_string(),
            blob_sha: oid.to_string(),
            language: language_name.to_string(),
            contracts: contracts.clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&entry) {
            let _ = fs_err::write(cache_path, json);
        }
    }
    Ok(contracts)
}

fn inline_cache_path(ctx: &RepoContext, oid: Oid, language: &str) -> Utf8PathBuf {
    ctx.inline_cache_dir()
        .join(format!("{oid}-{language}.json"))
}

fn inline_contracts_supported_for_path(path: &str) -> bool {
    !matches!(
        Utf8Path::new(path).extension(),
        Some("md" | "markdown" | "mdx" | "rst" | "txt")
    )
}

// attest: begin
// scope: function
// id: attest_core.parse_inline_contract_blob
// module: attest-core
// claims:
//   - id: attest_core.inline_parser_keeps_authoring_plain
//     text: Inline contract parsing keeps authoring plain enough that users can write contracts as comments without learning Attest-specific syntax tricks.
//     review:
//       - Explain the smallest example a user can copy into a source file.
//       - Identify any formatting rule that would surprise someone who knows YAML and the host language.
//       - Call out any parsing convenience that risks making contracts ambiguous.
//   - id: attest_core.inline_function_binding_has_obvious_ownership
//     text: Function-scoped inline contracts bind to code in a way that makes ownership obvious from source layout.
//     review:
//       - Explain why the contract binds to this function and not a neighboring item.
//       - Identify any language construct where the binding would be surprising.
//       - Call out whether file or script scope would communicate the product expectation better.
// attest: end
fn parse_inline_contract_blob(
    path: &str,
    bytes: &[u8],
    language: Option<LanguageSpec>,
) -> Result<Vec<CachedInlineContract>> {
    let source = std::str::from_utf8(bytes)
        .with_context(|| format!("{path} contains inline contracts but is not UTF-8"))?;
    let parsed_tree = parse_tree_for_language(path, bytes, language.clone())?;
    let comment_lines = parsed_tree
        .as_ref()
        .map(|(_spec, tree)| collect_comment_lines(tree.root_node()));
    let blocks = extract_inline_blocks(source, comment_lines.as_ref());
    if blocks.is_empty() {
        return Ok(Vec::new());
    }

    let line_starts = line_start_offsets(source);
    let mut functions = Vec::new();
    let needs_function_binding = blocks.iter().any(|block| {
        serde_yaml_ng::from_str::<InlineContractYaml>(&block.yaml)
            .ok()
            .and_then(|contract| contract.scope)
            .is_none_or(|scope| scope == InlineScope::Function)
    });
    if needs_function_binding {
        let Some((spec, tree)) = parsed_tree.as_ref() else {
            bail!(
                "{path}: function-scoped inline contracts require a supported Tree-sitter language"
            );
        };
        functions = collect_function_candidates(tree.root_node(), source, spec.function_kinds);
    }

    let mut contracts = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for block in blocks {
        let raw: InlineContractYaml = serde_yaml_ng::from_str(&block.yaml)
            .with_context(|| format!("{path}: invalid inline contract YAML"))?;
        let scope = raw.scope.unwrap_or_else(|| {
            if next_function_after(&functions, block.end_byte).is_some() {
                InlineScope::Function
            } else {
                InlineScope::File
            }
        });
        let target = match scope {
            InlineScope::File | InlineScope::Script => CachedSpan {
                start_line: 1,
                end_line: line_starts.len().max(1),
                start_byte: 0,
                end_byte: source.len(),
            },
            InlineScope::Function => {
                next_function_after(&functions, block.end_byte)
                    .with_context(|| {
                        format!(
                            "{path}: inline contract `{}` is not followed by a function",
                            raw.id
                        )
                    })?
                    .span
            }
        };
        let symbol = match scope {
            InlineScope::Function => next_function_after(&functions, block.end_byte)
                .and_then(|function| function.symbol.clone()),
            InlineScope::File | InlineScope::Script => None,
        };
        let module = raw.module.unwrap_or_else(|| raw.id.clone());
        let contract = Contract {
            version: 1,
            module: module.clone(),
            claims: raw.claims.clone(),
            required_checks: raw.required_checks.clone(),
        };
        let pseudo_path = Utf8PathBuf::from(format!("{path}#attest:{}", raw.id));
        validate_contract(&pseudo_path, &contract)?;
        if !seen_ids.insert(raw.id.clone()) {
            bail!("{path}: duplicate inline contract id `{}`", raw.id);
        }
        contracts.push(CachedInlineContract {
            id: raw.id.clone(),
            module,
            scope,
            digest: digest_inline_contract(&block.yaml, scope, symbol.as_deref()),
            comment_span: block.span,
            target_span: target,
            symbol,
            claims: raw.claims,
            required_checks: raw.required_checks,
        });
    }
    Ok(contracts)
}

fn parse_tree_for_language(
    path: &str,
    bytes: &[u8],
    language: Option<LanguageSpec>,
) -> Result<Option<(LanguageSpec, tree_sitter::Tree)>> {
    let Some(spec) = language else {
        return Ok(None);
    };
    let mut tree_parser = TsParser::new();
    tree_parser
        .set_language(&spec.language)
        .with_context(|| format!("failed to load Tree-sitter language {}", spec.name))?;
    let tree = tree_parser
        .parse(bytes, None)
        .with_context(|| format!("Tree-sitter failed to parse {path}"))?;
    Ok(Some((spec, tree)))
}

fn inline_contracts_by_id(
    path: Option<&str>,
    contracts: Vec<CachedInlineContract>,
) -> Result<BTreeMap<String, CachedInlineContract>> {
    let mut by_id = BTreeMap::new();
    for contract in contracts {
        if by_id.insert(contract.id.clone(), contract).is_some() {
            bail!(
                "{}: duplicate inline contract id",
                path.unwrap_or("<unknown>")
            );
        }
    }
    Ok(by_id)
}

// attest: begin
// scope: function
// id: attest_core.inline_contract_needs_attestation
// module: attest-core
// claims:
//   - id: attest_core.inline_activation_matches_reviewer_intuition
//     text: inline_contract_needs_attestation matches a reviewer's intuition about when a local code contract should be reconsidered.
//     review:
//       - Explain why each activation case represents a meaningful change in code, ownership, or claim text.
//       - Identify any activation case that feels like bureaucracy instead of judgment.
//       - Call out any non-activation case where a reviewer would still expect a signoff.
//   - id: attest_core.inline_activation_keeps_precision
//     text: inline_contract_needs_attestation keeps function contracts precise enough that teams will trust them instead of disabling them.
//     review:
//       - Explain why the changed-line boundary is the right product tradeoff.
//       - Identify any common edit that would now trigger too many local contracts.
//       - Call out whether a broader contract scope would be more honest for the expectation being expressed.
// attest: end
fn inline_contract_needs_attestation(
    change: &FileChange,
    old: Option<&CachedInlineContract>,
    new: Option<&CachedInlineContract>,
) -> bool {
    let (Some(old), Some(new)) = (old, new) else {
        return true;
    };
    if change.old_path != change.new_path {
        return true;
    }
    if old.digest != new.digest {
        return true;
    }
    ranges_intersect(&change.old_changed_lines, old.comment_span.line_range())
        || ranges_intersect(&change.old_changed_lines, old.target_span.line_range())
        || ranges_intersect(&change.new_changed_lines, new.comment_span.line_range())
        || ranges_intersect(&change.new_changed_lines, new.target_span.line_range())
}

fn active_inline_contract(file_path: &str, contract: &CachedInlineContract) -> ActiveContract {
    let target = ContractTarget {
        kind: contract.scope.target_kind(),
        path: file_path.to_string(),
        symbol: contract.symbol.clone(),
        start_line: contract.target_span.start_line,
        end_line: contract.target_span.end_line,
        contract_start_line: contract.comment_span.start_line,
        contract_end_line: contract.comment_span.end_line,
    };
    let scope = match (target.kind, target.symbol.as_deref()) {
        (ContractTargetKind::Function, Some(symbol)) => format!("{file_path}:{symbol}"),
        (ContractTargetKind::Function, None) => format!("{file_path}:<function>"),
        (ContractTargetKind::Script, _) => format!("{file_path}:script"),
        (ContractTargetKind::File, _) => file_path.to_string(),
    };
    ActiveContract {
        module: contract.module.clone(),
        path: inline_contract_path(file_path, &contract.id),
        source: ContractSource::Inline,
        scope,
        digest: contract.digest.clone(),
        target: Some(target),
        changed_files: vec![file_path.to_string()],
        claims: contract.claims.clone(),
        required_checks: contract.required_checks.clone(),
    }
}

fn inline_contract_path(file_path: &str, id: &str) -> Utf8PathBuf {
    Utf8PathBuf::from(format!("{file_path}#attest:{id}"))
}

fn ranges_intersect(ranges: &[LineRange], span: LineRange) -> bool {
    ranges.iter().any(|range| range.intersects(&span))
}

fn extract_inline_blocks(
    source: &str,
    comment_lines: Option<&BTreeSet<usize>>,
) -> Vec<InlineBlock> {
    let line_starts = line_start_offsets(source);
    let lines = source.lines().collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line_number = index + 1;
        if !line_can_host_inline_contract(comment_lines, line_number)
            || normalize_contract_comment_line(lines[index]) != "attest: begin"
        {
            index += 1;
            continue;
        }
        let start_line = line_number;
        let mut yaml_lines = Vec::new();
        index += 1;
        while index < lines.len() {
            if !line_can_host_inline_contract(comment_lines, index + 1) {
                break;
            }
            let normalized = normalize_contract_comment_line(lines[index]);
            if normalized == "attest: end" {
                let end_line = index + 1;
                blocks.push(InlineBlock {
                    yaml: yaml_lines.join("\n"),
                    span: CachedSpan {
                        start_line,
                        end_line,
                        start_byte: line_start_byte(&line_starts, start_line),
                        end_byte: line_end_byte(&line_starts, source.len(), end_line),
                    },
                    end_byte: line_end_byte(&line_starts, source.len(), end_line),
                });
                break;
            }
            yaml_lines.push(normalized);
            index += 1;
        }
        index += 1;
    }
    blocks
}

fn line_can_host_inline_contract(comment_lines: Option<&BTreeSet<usize>>, line: usize) -> bool {
    comment_lines.is_none_or(|comment_lines| comment_lines.contains(&line))
}

fn normalize_contract_comment_line(line: &str) -> String {
    let mut value = line.trim_start();
    for prefix in ["//!", "///", "//", "#!", "#", "/*", "*", "\"\"\"", "'''"] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.strip_prefix(' ').unwrap_or(stripped);
            break;
        }
    }
    for suffix in ["*/", "\"\"\"", "'''"] {
        let trimmed = value.trim_end();
        if let Some(stripped) = trimmed.strip_suffix(suffix) {
            value = stripped.trim_end();
            break;
        }
    }
    value.trim_end().to_string()
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn line_start_byte(line_starts: &[usize], line: usize) -> usize {
    line_starts
        .get(line.saturating_sub(1))
        .copied()
        .unwrap_or(0)
}

fn line_end_byte(line_starts: &[usize], source_len: usize, line: usize) -> usize {
    line_starts.get(line).copied().unwrap_or(source_len)
}

fn language_for_path(path: &str) -> Option<LanguageSpec> {
    let extension = Utf8Path::new(path).extension()?;
    match extension {
        "rs" => Some(LanguageSpec {
            name: "rust",
            language: tree_sitter_rust::LANGUAGE.into(),
            function_kinds: &["function_item"],
        }),
        "py" | "pyw" => Some(LanguageSpec {
            name: "python",
            language: tree_sitter_python::LANGUAGE.into(),
            function_kinds: &["decorated_definition", "function_definition"],
        }),
        "js" | "jsx" | "mjs" | "cjs" => Some(LanguageSpec {
            name: "javascript",
            language: tree_sitter_javascript::LANGUAGE.into(),
            function_kinds: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "arrow_function",
                "function",
            ],
        }),
        "ts" => Some(LanguageSpec {
            name: "typescript",
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            function_kinds: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "arrow_function",
                "function",
            ],
        }),
        "tsx" => Some(LanguageSpec {
            name: "tsx",
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
            function_kinds: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "arrow_function",
                "function",
            ],
        }),
        _ => None,
    }
}

fn collect_function_candidates(
    root: Node<'_>,
    source: &str,
    function_kinds: &[&str],
) -> Vec<FunctionCandidate> {
    let mut candidates = Vec::new();
    collect_function_candidates_inner(root, source, function_kinds, &mut candidates);
    candidates.sort_by_key(|candidate| candidate.span.start_byte);
    candidates
}

fn collect_comment_lines(root: Node<'_>) -> BTreeSet<usize> {
    let mut lines = BTreeSet::new();
    collect_comment_lines_inner(root, &mut lines);
    lines
}

fn collect_comment_lines_inner(node: Node<'_>, lines: &mut BTreeSet<usize>) {
    if is_comment_node(node.kind()) {
        for line in (node.start_position().row + 1)..=(node.end_position().row + 1) {
            lines.insert(line);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comment_lines_inner(child, lines);
    }
}

fn is_comment_node(kind: &str) -> bool {
    matches!(kind, "comment" | "line_comment" | "block_comment")
}

fn collect_function_candidates_inner(
    node: Node<'_>,
    source: &str,
    function_kinds: &[&str],
    candidates: &mut Vec<FunctionCandidate>,
) {
    if function_kinds.contains(&node.kind()) {
        candidates.push(FunctionCandidate {
            span: CachedSpan {
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            },
            symbol: function_symbol(node, source),
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_function_candidates_inner(child, source, function_kinds, candidates);
    }
}

fn next_function_after(functions: &[FunctionCandidate], byte: usize) -> Option<&FunctionCandidate> {
    functions
        .iter()
        .filter(|function| function.span.start_byte >= byte)
        .min_by_key(|function| function.span.start_byte)
}

fn function_symbol(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|node| node_text(node, source))
    {
        return Some(name.to_string());
    }
    let mut current = node.parent();
    while let Some(parent) = current {
        if let Some(name) = parent
            .child_by_field_name("name")
            .and_then(|node| node_text(node, source))
        {
            return Some(name.to_string());
        }
        current = parent.parent();
    }
    None
}

fn node_text<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    node.utf8_text(source.as_bytes()).ok()
}

fn digest_inline_contract(yaml: &str, scope: InlineScope, symbol: Option<&str>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"attest.inline-contract.v1\n");
    hasher.update(format!("{scope:?}\n").as_bytes());
    if let Some(symbol) = symbol {
        hasher.update(symbol.as_bytes());
    }
    hasher.update(b"\n");
    hasher.update(yaml.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn attestation_template(
    state: &CommitState,
    agent_kind: &str,
    agent_session: Option<String>,
) -> CommitAttestation {
    let mut items = Vec::new();
    for contract in &state.active_contracts {
        for claim in &contract.claims {
            items.push(AttestationItem {
                contract_path: contract.path.clone(),
                contract_digest: contract.digest.clone(),
                module: contract.module.clone(),
                source: contract.source,
                target: contract.target.clone(),
                claim_id: claim.id.clone(),
                claim: claim.text.clone(),
                review_questions: claim.review.clone(),
                changed_files: contract.changed_files.clone(),
                evidence_required: claim.evidence_required,
                status: None,
                evidence: Vec::new(),
            });
        }
    }
    CommitAttestation {
        kind: COMMIT_ATTESTATION_KIND.to_string(),
        generated_at: Timestamp::now().to_string(),
        base: state.base.clone(),
        base_sha: state.base_sha.clone(),
        staged_tree: state.staged_tree.clone(),
        staged_diff_digest: state.staged_diff_digest.clone(),
        signoff: AttestationSignoff {
            agent_kind: agent_kind.to_string(),
            agent_session,
            signed_at: None,
        },
        items,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftAction {
    Created,
    Replaced,
    Kept,
}

fn ensure_draft_for_state(
    ctx: &RepoContext,
    state: &CommitState,
    path: &Utf8Path,
    agent_kind: &str,
    agent_session: Option<String>,
    force: bool,
) -> Result<DraftAction> {
    let existing = classify_existing_draft(path, state)?;
    if existing == DraftAction::Kept && !force {
        return Ok(DraftAction::Kept);
    }

    ctx.ensure_attest_dir()?;
    let draft = attestation_template(state, agent_kind, agent_session);
    let yaml = attestation_yaml(&draft).context("failed to serialize attestation draft")?;
    fs_err::write(path, yaml).with_context(|| format!("failed to write {path}"))?;
    Ok(match existing {
        DraftAction::Created => DraftAction::Created,
        DraftAction::Kept | DraftAction::Replaced => DraftAction::Replaced,
    })
}

fn classify_existing_draft(path: &Utf8Path, state: &CommitState) -> Result<DraftAction> {
    let bytes = match fs_err::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DraftAction::Created);
        }
        Err(error) => return Err(error).with_context(|| format!("failed to read {path}")),
    };
    if let Ok(attestation) = serde_yaml_ng::from_slice::<CommitAttestation>(&bytes)
        && attestation_matches_state(state, &attestation)
    {
        return Ok(DraftAction::Kept);
    }
    if String::from_utf8_lossy(&bytes).contains(&state.staged_diff_digest) {
        return Ok(DraftAction::Kept);
    }
    Ok(DraftAction::Replaced)
}

fn attestation_yaml(attestation: &CommitAttestation) -> Result<String> {
    Ok(format!(
        "{}{}",
        attestation_instructions(),
        serde_yaml_ng::to_string(attestation)?
    ))
}

fn attestation_instructions() -> &'static str {
    "# Attest commit signoff\n\
#\n\
# This file blocks the current git commit until every required claim is reviewed.\n\
#\n\
# Required procedure:\n\
# 1. Inspect the staged diff for this commit.\n\
# 2. Read each contract and each changed file listed below.\n\
# 3. For each claim, set status to true only if the claim is actually satisfied.\n\
# 4. Add concrete evidence from the diff for every true claim.\n\
# 5. If any claim is false or unsure, do not sign. Report the blocker.\n\
# 6. Set signed_at after reviewing every claim.\n\
# 7. Run git commit again.\n\
#\n\
# Do not sign mechanically. This is not a checklist to rubber-stamp.\n\n"
}

fn read_attestation(path: &Utf8Path) -> Result<CommitAttestation> {
    let bytes = match fs_err::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "missing attestation draft at {path}; run `attest review` or `git commit` to create it"
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read attestation draft {path}"));
        }
    };
    serde_yaml_ng::from_slice(&bytes)
        .with_context(|| format!("failed to parse attestation draft {path}"))
}

fn attestation_matches_state(state: &CommitState, attestation: &CommitAttestation) -> bool {
    attestation.kind == COMMIT_ATTESTATION_KIND
        && attestation.base == state.base
        && attestation.base_sha == state.base_sha
        && attestation.staged_tree == state.staged_tree
        && attestation.staged_diff_digest == state.staged_diff_digest
}

fn verify_state(
    ctx: &RepoContext,
    state: &CommitState,
    attestation_path: Option<&Utf8PathBuf>,
) -> Result<VerifyReport> {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if state.active_contracts.is_empty() {
        return Ok(VerifyReport {
            verdict: Verdict::Accepted,
            blockers,
            warnings,
            required_contracts: state.active_contracts.clone(),
            attestation: None,
        });
    }

    let path = attestation_path
        .cloned()
        .unwrap_or_else(|| ctx.attestation_path());
    let attestation = match read_attestation(&path) {
        Ok(attestation) => Some(attestation),
        Err(error) => {
            blockers.push(format!("{error}"));
            None
        }
    };

    if let Some(attestation) = &attestation {
        verify_attestation_freshness(state, attestation, &mut blockers);
        verify_attestation_items(state, attestation, &mut blockers, &mut warnings);
    }

    let verdict = if blockers.is_empty() {
        Verdict::Accepted
    } else {
        Verdict::Blocked
    };

    Ok(VerifyReport {
        verdict,
        blockers,
        warnings,
        required_contracts: state.active_contracts.clone(),
        attestation: attestation.and_then(|attestation| {
            attestation
                .signoff
                .signed_at
                .map(|signed_at| AttestationSummary {
                    path,
                    signed_at,
                    agent_kind: attestation.signoff.agent_kind,
                    agent_session: attestation.signoff.agent_session,
                })
        }),
    })
}

fn verify_attestation_freshness(
    state: &CommitState,
    attestation: &CommitAttestation,
    blockers: &mut Vec<String>,
) {
    if attestation.kind != COMMIT_ATTESTATION_KIND {
        blockers.push(format!(
            "attestation kind `{}` is unsupported",
            attestation.kind
        ));
    }
    if attestation.base != state.base {
        blockers.push(format!(
            "attestation base is stale: signed {}, current {}",
            attestation.base, state.base
        ));
    }
    if attestation.base_sha != state.base_sha {
        blockers.push(format!(
            "attestation base sha is stale: signed {}, current {}",
            optional_sha(&attestation.base_sha),
            optional_sha(&state.base_sha)
        ));
    }
    if attestation.staged_tree != state.staged_tree {
        blockers.push(format!(
            "attestation staged tree is stale: signed {}, current {}",
            attestation.staged_tree, state.staged_tree
        ));
    }
    if attestation.staged_diff_digest != state.staged_diff_digest {
        blockers.push(format!(
            "attestation staged diff digest is stale: signed {}, current {}",
            attestation.staged_diff_digest, state.staged_diff_digest
        ));
    }
    if attestation.signoff.signed_at.is_none() {
        blockers.push("attestation signoff.signed_at is not set".to_string());
    }
}

fn verify_attestation_items(
    state: &CommitState,
    attestation: &CommitAttestation,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let mut expected = BTreeMap::<(Utf8PathBuf, String), (&ActiveContract, &Claim)>::new();
    for contract in &state.active_contracts {
        for claim in &contract.claims {
            expected.insert((contract.path.clone(), claim.id.clone()), (contract, claim));
        }
    }

    let mut actual = BTreeMap::<(Utf8PathBuf, String), &AttestationItem>::new();
    for item in &attestation.items {
        let key = (item.contract_path.clone(), item.claim_id.clone());
        if actual.insert(key, item).is_some() {
            blockers.push(format!(
                "duplicate attestation item for claim `{}` in {}",
                item.claim_id, item.contract_path
            ));
        }
    }

    for (key, (contract, claim)) in &expected {
        let Some(item) = actual.get(key) else {
            blockers.push(format!(
                "missing attestation item for claim `{}` in {}",
                claim.id, contract.path
            ));
            continue;
        };
        if item.contract_digest != contract.digest {
            blockers.push(format!(
                "contract {} changed since draft creation: signed {}, current {}",
                contract.path, item.contract_digest, contract.digest
            ));
        }
        if item.status != Some(ClaimStatus::True) {
            blockers.push(format!(
                "claim `{}` in {} is not signed true",
                claim.id, contract.path
            ));
        }
        if claim.evidence_required && clean_evidence(&item.evidence).is_empty() {
            blockers.push(format!(
                "claim `{}` in {} requires evidence",
                claim.id, contract.path
            ));
        }
    }

    for (key, item) in actual {
        if !expected.contains_key(&key) {
            warnings.push(format!(
                "attestation contains non-required claim `{}` in {}",
                item.claim_id, item.contract_path
            ));
        }
    }
}

fn pre_commit_block_message(
    ctx: &RepoContext,
    state: &CommitState,
    path: &Utf8Path,
    action: DraftAction,
) -> String {
    let mut message =
        String::from("You need to sign these Attest contracts before committing:\n\n");
    for contract in &state.active_contracts {
        message.push_str(&format!("- {}\n", contract.path));
        for claim in &contract.claims {
            message.push_str(&format!("  - {}\n", claim.id));
        }
    }
    let path = display_path(ctx, path);
    let line = match action {
        DraftAction::Created | DraftAction::Replaced => "Attestation draft written to:",
        DraftAction::Kept => "Attestation draft is waiting at:",
    };
    message.push_str(&format!(
        "\n{line}\n  {path}\n\nFollow the instructions at the top of that file, then run:\n  git commit\n"
    ));
    message
}

fn display_path(ctx: &RepoContext, path: &Utf8Path) -> String {
    path.strip_prefix(&ctx.root)
        .map(|path| path.to_string())
        .unwrap_or_else(|_| path.to_string())
}

fn print_state(state: &CommitState) {
    match &state.base_sha {
        Some(base_sha) => println!("Base: HEAD ({})", short_sha(base_sha)),
        None => println!("Base: HEAD (unborn)"),
    }
    println!("Staged tree: {}", short_sha(&state.staged_tree));
    println!("Staged diff: {}", state.staged_diff_digest);
    if state.changed_files.is_empty() {
        println!("Staged files: none");
    } else {
        println!("Staged files:");
        for file in &state.changed_files {
            println!("  - {file}");
        }
    }
    if !state.unstaged_files.is_empty() {
        println!("Unstaged files not covered by this attestation:");
        for file in &state.unstaged_files {
            println!("  - {file}");
        }
    }
    if state.active_contracts.is_empty() {
        println!("Required contracts: none");
    } else {
        println!("Required contracts:");
        for contract in &state.active_contracts {
            let scope = if contract.scope.is_empty() {
                "<repo>"
            } else {
                contract.scope.as_str()
            };
            println!(
                "  - {} ({}, scope: {scope})",
                contract.module, contract.path
            );
            if let Some(target) = &contract.target {
                match (target.kind, target.symbol.as_deref()) {
                    (ContractTargetKind::Function, Some(symbol)) => println!(
                        "      target: function {symbol} lines {}-{}",
                        target.start_line, target.end_line
                    ),
                    (ContractTargetKind::Function, None) => println!(
                        "      target: function lines {}-{}",
                        target.start_line, target.end_line
                    ),
                    (ContractTargetKind::File, _) => println!(
                        "      target: file lines {}-{}",
                        target.start_line, target.end_line
                    ),
                    (ContractTargetKind::Script, _) => println!(
                        "      target: script lines {}-{}",
                        target.start_line, target.end_line
                    ),
                }
            }
            for claim in &contract.claims {
                println!("      - {}", claim.id);
            }
        }
    }
}

fn print_report(report: &VerifyReport) {
    match report.verdict {
        Verdict::Accepted => println!("Attest verification: accepted"),
        Verdict::Blocked => println!("Attest verification: blocked"),
    }
    for blocker in &report.blockers {
        println!("blocker: {blocker}");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

impl std::fmt::Display for ClaimStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimStatus::True => write!(f, "true"),
            ClaimStatus::False => write!(f, "false"),
            ClaimStatus::Unsure => write!(f, "unsure"),
        }
    }
}

fn normalize_std_path(path: &Path) -> Option<String> {
    let path = Utf8Path::from_path(path)?;
    Some(path.as_str().replace('\\', "/"))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn clean_evidence(evidence: &[String]) -> Vec<String> {
    evidence
        .iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn optional_sha(sha: &Option<String>) -> String {
    sha.as_deref().unwrap_or("<none>").to_string()
}

fn default_contract_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn default_agent_kind() -> String {
    "codex".to_string()
}

#[cfg(unix)]
fn make_executable(path: &Utf8Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs_err::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs_err::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Utf8Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_digest_changes_when_contract_bytes_change() {
        let one = digest_bytes(b"module: engine\n");
        let two = digest_bytes(b"module: planner\n");
        assert_ne!(one, two);
    }

    #[test]
    fn directory_scope_matches_descendants() {
        assert!(path_is_within_scope("README.md", ""));
        assert!(path_is_within_scope("crates/engine", "crates/engine"));
        assert!(path_is_within_scope(
            "crates/engine/src/lib.rs",
            "crates/engine"
        ));
        assert!(!path_is_within_scope(
            "crates/planner/src/lib.rs",
            "crates/engine"
        ));
    }

    #[test]
    fn generated_attestation_starts_with_agent_instructions() {
        let attestation = CommitAttestation {
            kind: COMMIT_ATTESTATION_KIND.to_string(),
            generated_at: Timestamp::now().to_string(),
            base: "HEAD".to_string(),
            base_sha: None,
            staged_tree: "tree".to_string(),
            staged_diff_digest: "blake3:digest".to_string(),
            signoff: AttestationSignoff {
                agent_kind: "codex".to_string(),
                agent_session: None,
                signed_at: None,
            },
            items: Vec::new(),
        };
        let yaml = attestation_yaml(&attestation).unwrap();
        assert!(yaml.starts_with("# Attest commit signoff"));
        assert!(yaml.contains("If any claim is false or unsure, do not sign. Report the blocker."));
        assert!(yaml.contains("agent_session: null"));
        assert!(yaml.contains("signed_at: null"));
    }
}
