use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand, ValueEnum};
use git2::{DiffFormat, DiffOptions, Oid, Repository, StatusOptions};
use inquire::{Select, Text};
use jiff::Timestamp;
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::Path;
use tree_sitter::{Language, Node, Parser as TsParser};

const CONTRACT_FILENAMES: &[&str] = &[
    "AGENT_CONTRACT.yaml",
    "AGENT_CONTRACT.yml",
    "attest.yaml",
    "attest.yml",
];

const ATTEST_DIR: &str = "attest";
const ATTESTATION_FILE: &str = "pr-attestation.json";
const REVIEW_FILE: &str = "pr-review.yaml";
const INLINE_CACHE_VERSION: &str = "attest.inline-cache.v1";
const INLINE_BEGIN: &[u8] = b"attest: begin";

#[derive(Debug, Parser)]
#[command(
    name = "attest",
    version,
    about = "Per-PR contract attestations for coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a starter AGENT_CONTRACT.yaml.
    Init(InitArgs),
    /// Show required contracts for the current PR diff.
    Status(StatusArgs),
    /// Generate a review worksheet the agent must fill before signing.
    ReviewPr(ReviewArgs),
    /// Sign the current PR diff after reviewing every active claim.
    SignPr(SignArgs),
    /// Verify the current PR attestation is fresh and complete.
    VerifyPr(VerifyArgs),
    /// Codex Stop hook entrypoint. Emits hook JSON.
    CodexStopHook(HookArgs),
    /// Install Codex and/or Git hooks for this repository.
    InstallHooks(InstallHookArgs),
    /// Inspect inline comment contracts in source files.
    Inline(InlineArgs),
    /// Emit JSON schema for contract, review, or attestation files.
    Schema(SchemaArgs),
    /// Run Attest as a stdio MCP server.
    McpServer,
}

#[derive(Debug, Args, Clone)]
struct RepoArgs {
    /// Repository path. Defaults to the current directory.
    #[arg(long)]
    repo: Option<Utf8PathBuf>,
}

#[derive(Debug, Args, Clone)]
struct PrArgs {
    #[command(flatten)]
    repo: RepoArgs,

    /// Base branch/ref for the PR diff.
    #[arg(long, default_value = "origin/main", env = "ATTEST_BASE")]
    base: String,
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
    pr: PrArgs,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReviewArgs {
    #[command(flatten)]
    pr: PrArgs,

    /// Review worksheet path. Defaults to .git/attest/pr-review.yaml.
    #[arg(long)]
    output: Option<Utf8PathBuf>,

    /// Also print the generated review worksheet.
    #[arg(long)]
    print: bool,
}

#[derive(Debug, Args)]
struct SignArgs {
    #[command(flatten)]
    pr: PrArgs,

    /// Read reviewed answers from a worksheet. Defaults to .git/attest/pr-review.yaml when present.
    #[arg(long)]
    from_review: Option<Option<Utf8PathBuf>>,

    /// Agent kind recorded in the attestation.
    #[arg(long, default_value = "codex", env = "ATTEST_AGENT_KIND")]
    agent_kind: String,

    /// Agent/session identifier recorded in the attestation.
    #[arg(long, env = "ATTEST_AGENT_SESSION")]
    session_id: Option<String>,

    /// Attestation output path. Defaults to .git/attest/pr-attestation.json.
    #[arg(long)]
    output: Option<Utf8PathBuf>,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    #[command(flatten)]
    pr: PrArgs,

    /// Attestation path. Defaults to .git/attest/pr-attestation.json.
    #[arg(long)]
    attestation: Option<Utf8PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HookArgs {
    #[command(flatten)]
    pr: PrArgs,

    /// Treat repository/base setup errors as no-op. Useful for globally enabled plugin hooks.
    #[arg(long)]
    soft: bool,
}

#[derive(Debug, Args)]
struct InstallHookArgs {
    #[command(flatten)]
    pr: PrArgs,

    /// Install the Codex Stop hook in .codex/hooks.json.
    #[arg(long)]
    codex: bool,

    /// Install the Git pre-push hook in .git/hooks/pre-push.
    #[arg(long)]
    git: bool,

    /// Overwrite existing hook files.
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
    Review,
    Attestation,
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init(args) => init(args),
        Command::Status(args) => status(args),
        Command::ReviewPr(args) => review_pr(args),
        Command::SignPr(args) => sign_pr(args),
        Command::VerifyPr(args) => verify_pr(args),
        Command::CodexStopHook(args) => codex_stop_hook(args),
        Command::InstallHooks(args) => install_hooks(args),
        Command::Inline(args) => inline(args),
        Command::Schema(args) => schema(args),
        Command::McpServer => {
            let runtime =
                tokio::runtime::Runtime::new().context("failed to start tokio runtime")?;
            runtime.block_on(run_mcp_server())
        }
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
pub struct PrState {
    pub base_ref: String,
    pub base_sha: String,
    pub merge_base_sha: String,
    pub head_sha: String,
    pub diff_digest: String,
    pub changed_files: Vec<String>,
    pub dirty_files: Vec<String>,
    pub active_contracts: Vec<ActiveContract>,
    #[schemars(with = "String")]
    pub attestation_path: Utf8PathBuf,
    #[schemars(with = "String")]
    pub review_path: Utf8PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFile {
    pub kind: String,
    pub generated_at: String,
    pub base_ref: String,
    pub base_sha: String,
    pub merge_base_sha: String,
    pub head_sha: String,
    pub diff_digest: String,
    pub items: Vec<ReviewItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewItem {
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
pub struct Attestation {
    pub kind: String,
    pub signed_at: String,
    pub base_ref: String,
    pub base_sha: String,
    pub merge_base_sha: String,
    pub head_sha: String,
    pub diff_digest: String,
    pub agent: Agent,
    pub contracts: Vec<ContractSignoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Agent {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractSignoff {
    #[schemars(with = "String")]
    pub path: Utf8PathBuf,
    pub digest: String,
    pub module: String,
    #[serde(default)]
    pub source: ContractSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ContractTarget>,
    pub claims: Vec<ClaimSignoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaimSignoff {
    pub id: String,
    pub status: ClaimStatus,
    pub evidence: Vec<String>,
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
    pub signed_at: String,
    pub agent: Agent,
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

    fn review_path(&self) -> Utf8PathBuf {
        self.attest_dir().join(REVIEW_FILE)
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
  - id: repo.no_test_case_heuristics
    text: I did not add logic specific to a single test fixture.
    review:
      - List each production branch added or changed.
      - Explain why each branch generalizes beyond the regression test.
      - Name the test or check that would fail if the logic were hard-coded.
  - id: repo.public_surface_intentional
    text: I did not add public API surface unless this change requires it.
    review:
      - List any new public item, endpoint, type, or command.
      - Explain why each public addition belongs in this change.
"#
}

fn status(args: StatusArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.pr.repo.repo)?;
    let state = build_pr_state(&ctx, &args.pr.base)?;
    let report = verify_state(&ctx, &state, None)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_state(&state);
    print_report(&report);
    Ok(())
}

fn review_pr(args: ReviewArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.pr.repo.repo)?;
    let state = build_pr_state(&ctx, &args.pr.base)?;
    let review = review_template(&state);
    let output = args.output.unwrap_or_else(|| ctx.review_path());

    ctx.ensure_attest_dir()?;
    let yaml = serde_yaml_ng::to_string(&review).context("failed to serialize review worksheet")?;
    fs_err::write(&output, &yaml).with_context(|| format!("failed to write {output}"))?;

    println!("Wrote review worksheet to {output}");
    if state.active_contracts.is_empty() {
        println!("No contracts apply to the current PR diff.");
    } else {
        println!(
            "Review {} claim(s), set every status to true, and add evidence before signing.",
            review.items.len()
        );
    }
    if args.print {
        println!("\n{yaml}");
    }
    Ok(())
}

fn sign_pr(args: SignArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.pr.repo.repo)?;
    let state = build_pr_state(&ctx, &args.pr.base)?;
    let review_path = match args.from_review {
        Some(Some(path)) => Some(path),
        Some(None) => Some(ctx.review_path()),
        None if ctx.review_path().exists() => Some(ctx.review_path()),
        None => None,
    };

    let signoffs = if let Some(path) = review_path {
        let review = read_review(&path)?;
        signoffs_from_review(&state, &review)?
    } else {
        if !std::io::stdin().is_terminal() {
            bail!(
                "non-interactive signing requires a review worksheet. Run `attest review-pr --base {}` first.",
                args.pr.base
            );
        }
        prompt_for_signoffs(&state)?
    };

    let attestation = Attestation {
        kind: "attest.pr.v1".to_string(),
        signed_at: Timestamp::now().to_string(),
        base_ref: state.base_ref.clone(),
        base_sha: state.base_sha.clone(),
        merge_base_sha: state.merge_base_sha.clone(),
        head_sha: state.head_sha.clone(),
        diff_digest: state.diff_digest.clone(),
        agent: Agent {
            kind: args.agent_kind,
            session_id: args.session_id,
        },
        contracts: signoffs,
    };

    let output = args.output.unwrap_or_else(|| ctx.attestation_path());
    ctx.ensure_attest_dir()?;
    fs_err::write(&output, serde_json::to_string_pretty(&attestation)?)
        .with_context(|| format!("failed to write {output}"))?;
    println!("Signed PR attestation at {output}");
    Ok(())
}

fn verify_pr(args: VerifyArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.pr.repo.repo)?;
    let state = build_pr_state(&ctx, &args.pr.base)?;
    let report = verify_state(&ctx, &state, args.attestation.as_ref())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if report.verdict == Verdict::Blocked {
        bail!("PR attestation verification failed");
    }
    Ok(())
}

fn codex_stop_hook(args: HookArgs) -> Result<()> {
    let ctx = match RepoContext::open(&args.pr.repo.repo) {
        Ok(ctx) => ctx,
        Err(error) if args.soft => {
            eprintln!("Attest soft hook skipped: {error}");
            println!("{{}}");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let state = match build_pr_state(&ctx, &args.pr.base) {
        Ok(state) => state,
        Err(error) if args.soft => {
            eprintln!("Attest soft hook skipped: {error}");
            println!("{{}}");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let report = verify_state(&ctx, &state, None)?;

    let mut reasons = Vec::new();
    if !state.dirty_files.is_empty() {
        reasons.push(format!(
            "The working tree has uncommitted changes: {}. Attest signs the committed PR diff, so commit or stash those changes before signing.",
            state.dirty_files.join(", ")
        ));
    }
    reasons.extend(report.blockers.clone());

    if reasons.is_empty() {
        println!("{{}}");
        return Ok(());
    }

    let reason = format!(
        "{}\n\nDo not sign mechanically. Run `attest status --base {base}` and `attest review-pr --base {base}`. Inspect each changed module contract and matching diff. If any claim is false or uncertain, fix the code or report the blocker. Only after every required claim is true, fill the review worksheet, run `attest sign-pr --base {base} --from-review`, then run `attest verify-pr --base {base}`.",
        reasons.join("\n"),
        base = args.pr.base
    );
    let payload = serde_json::json!({
        "continue": false,
        "stopReason": "Attest PR signoff is stale",
        "systemMessage": reason
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn install_hooks(args: InstallHookArgs) -> Result<()> {
    let ctx = RepoContext::open(&args.pr.repo.repo)?;
    let install_codex = args.codex || !args.git;
    let install_git = args.git || !args.codex;

    if install_codex {
        let hooks_path = write_codex_hook(&ctx, &args.pr.base, args.force)?;
        println!("Installed Codex Stop hook at {hooks_path}");
    }

    if install_git {
        let hook_path = write_git_hook(&ctx, &args.pr.base, args.force)?;
        println!("Installed Git pre-push hook at {hook_path}");
    }

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

fn write_codex_hook(ctx: &RepoContext, base: &str, force: bool) -> Result<Utf8PathBuf> {
    let codex_dir = ctx.root.join(".codex");
    fs_err::create_dir_all(&codex_dir).with_context(|| format!("failed to create {codex_dir}"))?;
    let hooks_path = codex_dir.join("hooks.json");
    if hooks_path.exists() && !force {
        bail!("{hooks_path} already exists. Re-run with --force to overwrite it.");
    }
    let hook = serde_json::json!({
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("attest codex-stop-hook --base {}", shell_quote(base)),
                            "timeout": 30,
                            "statusMessage": "Checking Attest PR signoff"
                        }
                    ]
                }
            ]
        }
    });
    fs_err::write(&hooks_path, serde_json::to_string_pretty(&hook)?)
        .with_context(|| format!("failed to write {hooks_path}"))?;
    Ok(hooks_path)
}

fn write_git_hook(ctx: &RepoContext, base: &str, force: bool) -> Result<Utf8PathBuf> {
    let hook_path = ctx.git_dir.join("hooks").join("pre-push");
    if hook_path.exists() && !force {
        bail!("{hook_path} already exists. Re-run with --force to overwrite it.");
    }
    let hook = format!(
        "#!/bin/sh\nset -eu\nattest verify-pr --base {}\n",
        shell_quote(base)
    );
    fs_err::write(&hook_path, hook).with_context(|| format!("failed to write {hook_path}"))?;
    make_executable(&hook_path)?;
    Ok(hook_path)
}

fn schema(args: SchemaArgs) -> Result<()> {
    let value = match args.kind {
        SchemaKind::Contract => serde_json::to_value(schema_for!(Contract))?,
        SchemaKind::Review => serde_json::to_value(schema_for!(ReviewFile))?,
        SchemaKind::Attestation => serde_json::to_value(schema_for!(Attestation))?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn run_mcp_server() -> Result<()> {
    AttestMcpServer::new()
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start Attest MCP server")?
        .waiting()
        .await
        .context("Attest MCP server failed")?;
    Ok(())
}

#[derive(Debug, Clone)]
struct AttestMcpServer {
    tool_router: ToolRouter<Self>,
}

impl AttestMcpServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpPrParams {
    #[schemars(description = "Repository path. Defaults to the current working directory.")]
    repo: Option<String>,
    #[schemars(description = "PR base ref. Defaults to origin/main.")]
    base: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpReviewParams {
    #[schemars(description = "Repository path. Defaults to the current working directory.")]
    repo: Option<String>,
    #[schemars(description = "PR base ref. Defaults to origin/main.")]
    base: Option<String>,
    #[schemars(description = "Optional output path. Defaults to .git/attest/pr-review.yaml.")]
    output: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpSignParams {
    #[schemars(description = "Repository path. Defaults to the current working directory.")]
    repo: Option<String>,
    #[schemars(description = "PR base ref. Defaults to origin/main.")]
    base: Option<String>,
    #[schemars(description = "Agent kind recorded in the attestation. Defaults to codex.")]
    agent_kind: Option<String>,
    #[schemars(description = "Agent/session identifier recorded in the attestation.")]
    session_id: Option<String>,
    #[schemars(
        description = "Reviewed claim answers. Every active claim must be true with evidence."
    )]
    claims: Vec<McpClaimReview>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct McpClaimReview {
    #[schemars(
        description = "Contract path, for example AGENT_CONTRACT.yaml or crates/engine/AGENT_CONTRACT.yaml."
    )]
    contract_path: String,
    #[schemars(description = "Claim id from the contract.")]
    claim_id: String,
    #[schemars(description = "Must be true to sign. false and unsure are rejected.")]
    status: ClaimStatus,
    #[schemars(description = "Evidence from reviewing the diff. Required by default.")]
    evidence: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpInitParams {
    #[schemars(description = "Repository path. Defaults to the current working directory.")]
    repo: Option<String>,
    #[schemars(description = "Overwrite an existing root AGENT_CONTRACT.yaml.")]
    force: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpInstallHooksParams {
    #[schemars(description = "Repository path. Defaults to the current working directory.")]
    repo: Option<String>,
    #[schemars(description = "PR base ref. Defaults to origin/main.")]
    base: Option<String>,
    #[schemars(
        description = "Install the Codex Stop hook. If neither hook flag is true, both are installed."
    )]
    codex: Option<bool>,
    #[schemars(
        description = "Install the Git pre-push hook. If neither hook flag is true, both are installed."
    )]
    git: Option<bool>,
    #[schemars(description = "Overwrite existing hook files.")]
    force: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct McpInlineExplainParams {
    #[schemars(description = "Repository path. Defaults to the current working directory.")]
    repo: Option<String>,
    #[schemars(description = "Source file path, relative to the repository root.")]
    path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpStatusOutput {
    state: PrState,
    verification: VerifyReport,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpReviewOutput {
    path: String,
    review: ReviewFile,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpSignOutput {
    path: String,
    attestation: Attestation,
    verification: VerifyReport,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpPathOutput {
    path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpInstallHooksOutput {
    codex_hook: Option<String>,
    git_hook: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpInlineExplainOutput {
    path: String,
    contracts: Vec<McpInlineExplainContract>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpInlineExplainContract {
    id: String,
    module: String,
    target: ContractTarget,
    claims: Vec<Claim>,
}

#[tool_router]
impl AttestMcpServer {
    #[tool(
        name = "status_pr",
        description = "Inspect the current PR diff, active directory/inline contracts, and attestation freshness."
    )]
    fn status_pr(
        &self,
        Parameters(params): Parameters<McpPrParams>,
    ) -> Result<Json<McpStatusOutput>, String> {
        let (ctx, state) = mcp_state(params.repo, params.base)?;
        let verification = verify_state(&ctx, &state, None).map_err(tool_error)?;
        Ok(Json(McpStatusOutput {
            state,
            verification,
        }))
    }

    #[tool(
        name = "create_review",
        description = "Create a PR review worksheet with every active contract claim that must be reviewed before signing."
    )]
    fn create_review(
        &self,
        Parameters(params): Parameters<McpReviewParams>,
    ) -> Result<Json<McpReviewOutput>, String> {
        let (ctx, state) = mcp_state(params.repo, params.base)?;
        let review = review_template(&state);
        let output = match params.output {
            Some(path) => Utf8PathBuf::from(path),
            None => ctx.review_path(),
        };

        ctx.ensure_attest_dir().map_err(tool_error)?;
        let yaml = serde_yaml_ng::to_string(&review).map_err(tool_error)?;
        fs_err::write(&output, yaml).map_err(tool_error)?;

        Ok(Json(McpReviewOutput {
            path: output.to_string(),
            review,
        }))
    }

    #[tool(
        name = "sign_pr",
        description = "Sign the current PR attestation from reviewed claim answers. Rejects false, unsure, missing, or evidence-free claims."
    )]
    fn sign_pr_tool(
        &self,
        Parameters(params): Parameters<McpSignParams>,
    ) -> Result<Json<McpSignOutput>, String> {
        let (ctx, state) = mcp_state(params.repo, params.base)?;
        let mut review = review_template(&state);
        apply_mcp_claim_reviews(&mut review, params.claims)?;
        let signoffs = signoffs_from_review(&state, &review).map_err(tool_error)?;

        let attestation = Attestation {
            kind: "attest.pr.v1".to_string(),
            signed_at: Timestamp::now().to_string(),
            base_ref: state.base_ref.clone(),
            base_sha: state.base_sha.clone(),
            merge_base_sha: state.merge_base_sha.clone(),
            head_sha: state.head_sha.clone(),
            diff_digest: state.diff_digest.clone(),
            agent: Agent {
                kind: params.agent_kind.unwrap_or_else(|| "codex".to_string()),
                session_id: params.session_id,
            },
            contracts: signoffs,
        };

        let output = ctx.attestation_path();
        ctx.ensure_attest_dir().map_err(tool_error)?;
        fs_err::write(
            &output,
            serde_json::to_string_pretty(&attestation).map_err(tool_error)?,
        )
        .map_err(tool_error)?;
        let verification = verify_state(&ctx, &state, None).map_err(tool_error)?;

        Ok(Json(McpSignOutput {
            path: output.to_string(),
            attestation,
            verification,
        }))
    }

    #[tool(
        name = "verify_pr",
        description = "Verify that the current PR attestation is fresh and complete."
    )]
    fn verify_pr_tool(
        &self,
        Parameters(params): Parameters<McpPrParams>,
    ) -> Result<Json<VerifyReport>, String> {
        let (ctx, state) = mcp_state(params.repo, params.base)?;
        verify_state(&ctx, &state, None)
            .map(Json)
            .map_err(tool_error)
    }

    #[tool(
        name = "init_contract",
        description = "Create a starter root AGENT_CONTRACT.yaml."
    )]
    fn init_contract(
        &self,
        Parameters(params): Parameters<McpInitParams>,
    ) -> Result<Json<McpPathOutput>, String> {
        let repo = repo_arg(params.repo)?;
        let ctx = RepoContext::open(&repo).map_err(tool_error)?;
        let path = ctx.root.join("AGENT_CONTRACT.yaml");
        if path.exists() && !params.force.unwrap_or(false) {
            return Err(format!(
                "{path} already exists. Re-run with force=true to overwrite it."
            ));
        }
        fs_err::write(&path, sample_contract()).map_err(tool_error)?;
        Ok(Json(McpPathOutput {
            path: path.to_string(),
        }))
    }

    #[tool(
        name = "install_hooks",
        description = "Install Attest Codex Stop and/or Git pre-push hooks for this repository."
    )]
    fn install_hooks_tool(
        &self,
        Parameters(params): Parameters<McpInstallHooksParams>,
    ) -> Result<Json<McpInstallHooksOutput>, String> {
        let repo = repo_arg(params.repo)?;
        let ctx = RepoContext::open(&repo).map_err(tool_error)?;
        let codex = params.codex.unwrap_or(false);
        let git = params.git.unwrap_or(false);
        let install_codex = codex || !git;
        let install_git = git || !codex;
        let base = params.base.unwrap_or_else(default_base);
        let force = params.force.unwrap_or(false);

        let codex_hook = if install_codex {
            Some(
                write_codex_hook(&ctx, &base, force)
                    .map_err(tool_error)?
                    .to_string(),
            )
        } else {
            None
        };
        let git_hook = if install_git {
            Some(
                write_git_hook(&ctx, &base, force)
                    .map_err(tool_error)?
                    .to_string(),
            )
        } else {
            None
        };

        Ok(Json(McpInstallHooksOutput {
            codex_hook,
            git_hook,
        }))
    }

    #[tool(
        name = "explain_inline",
        description = "Validate inline contracts in a source file and return the file/function bindings."
    )]
    fn explain_inline(
        &self,
        Parameters(params): Parameters<McpInlineExplainParams>,
    ) -> Result<Json<McpInlineExplainOutput>, String> {
        let repo = repo_arg(params.repo)?;
        let ctx = RepoContext::open(&repo).map_err(tool_error)?;
        let relative =
            repo_relative_path(&ctx, &Utf8PathBuf::from(params.path)).map_err(tool_error)?;
        let contracts = inline_contracts_from_worktree(&ctx, &relative).map_err(tool_error)?;
        let contracts = contracts
            .into_iter()
            .map(|contract| {
                let target = ContractTarget {
                    kind: contract.scope.target_kind(),
                    path: relative.to_string(),
                    symbol: contract.symbol.clone(),
                    start_line: contract.target_span.start_line,
                    end_line: contract.target_span.end_line,
                    contract_start_line: contract.comment_span.start_line,
                    contract_end_line: contract.comment_span.end_line,
                };
                McpInlineExplainContract {
                    id: contract.id,
                    module: contract.module,
                    target,
                    claims: contract.claims,
                }
            })
            .collect();
        Ok(Json(McpInlineExplainOutput {
            path: relative.to_string(),
            contracts,
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AttestMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Attest enforces directory and inline PR contract signoffs. Use status_pr first. Before calling sign_pr, inspect the diff and only mark claims true when they are actually true and supported by evidence.",
        )
    }
}

fn mcp_state(repo: Option<String>, base: Option<String>) -> Result<(RepoContext, PrState), String> {
    let repo = repo_arg(repo)?;
    let ctx = RepoContext::open(&repo).map_err(tool_error)?;
    let state = build_pr_state(&ctx, &base.unwrap_or_else(default_base)).map_err(tool_error)?;
    Ok((ctx, state))
}

fn repo_arg(repo: Option<String>) -> Result<Option<Utf8PathBuf>, String> {
    Ok(repo.map(Utf8PathBuf::from))
}

fn default_base() -> String {
    "origin/main".to_string()
}

fn apply_mcp_claim_reviews(
    review: &mut ReviewFile,
    claims: Vec<McpClaimReview>,
) -> Result<(), String> {
    let expected = review
        .items
        .iter()
        .map(|item| (item.contract_path.to_string(), item.claim_id.clone()))
        .collect::<BTreeSet<_>>();
    let mut provided = BTreeMap::new();
    for claim in claims {
        let key = (claim.contract_path.clone(), claim.claim_id.clone());
        if !expected.contains(&key) {
            return Err(format!(
                "claim `{}` for contract `{}` is not required by the current PR diff",
                claim.claim_id, claim.contract_path
            ));
        }
        if provided.insert(key, claim).is_some() {
            return Err("duplicate reviewed claim in sign_pr request".to_string());
        }
    }

    for item in &mut review.items {
        let key = (item.contract_path.to_string(), item.claim_id.clone());
        if let Some(claim) = provided.remove(&key) {
            item.status = Some(claim.status);
            item.evidence = claim.evidence;
        }
    }
    Ok(())
}

fn tool_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

// attest: begin
// scope: function
// id: attest_core.build_pr_state
// module: attest-core
// claims:
//   - id: attest_core.build_pr_state_preserves_pr_mental_model
//     text: build_pr_state preserves the user's mental model that Attest is reviewing one committed PR diff, not a mixture of working tree, cache, and branch state.
//     review:
//       - Explain the product meaning of base, merge-base, head, changed files, and dirty files in this change.
//       - Identify any state that is observed but intentionally not signed.
//       - Call out any behavior that would surprise a user comparing their branch to the chosen base.
//   - id: attest_core.build_pr_state_keeps_activation_explainable
//     text: build_pr_state keeps contract activation explainable from changed paths and changed spans rather than hidden global repository state.
//     review:
//       - Explain why each newly active contract would make sense to a maintainer looking at the diff.
//       - Identify any contract activation that depends on ordering, caching, or unrelated files.
//       - Call out any behavior that feels conservative enough for pre-push but too noisy for routine use.
// attest: end
fn build_pr_state(ctx: &RepoContext, base_ref: &str) -> Result<PrState> {
    let base_commit = resolve_commit(&ctx.repo, base_ref)
        .with_context(|| format!("failed to resolve base ref `{base_ref}`"))?;
    let head_commit = ctx
        .repo
        .head()?
        .peel_to_commit()
        .context("failed to resolve HEAD")?;
    let merge_base = ctx.repo.merge_base(base_commit.id(), head_commit.id())?;
    let merge_base_commit = ctx.repo.find_commit(merge_base)?;

    let diff = diff_between(&ctx.repo, merge_base_commit.id(), head_commit.id())?;
    let file_changes = collect_file_changes(&diff)?;
    let changed_files = changed_files_from_changes(&file_changes);
    let diff_digest = diff_digest(
        &diff,
        merge_base_commit.id(),
        head_commit.id(),
        &changed_files,
    )?;

    let contracts = load_directory_contracts_for_paths(ctx, &changed_files)?;
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

    Ok(PrState {
        base_ref: base_ref.to_string(),
        base_sha: base_commit.id().to_string(),
        merge_base_sha: merge_base_commit.id().to_string(),
        head_sha: head_commit.id().to_string(),
        diff_digest,
        changed_files,
        dirty_files: dirty_files(ctx)?,
        active_contracts,
        attestation_path: ctx.attestation_path(),
        review_path: ctx.review_path(),
    })
}

fn resolve_commit<'repo>(repo: &'repo Repository, refspec: &str) -> Result<git2::Commit<'repo>> {
    let object = repo.revparse_single(refspec)?;
    object
        .peel_to_commit()
        .context("ref did not resolve to a commit")
}

fn diff_between(repo: &Repository, old: Oid, new: Oid) -> Result<git2::Diff<'_>> {
    let old_tree = repo.find_commit(old)?.tree()?;
    let new_tree = repo.find_commit(new)?.tree()?;
    let mut options = DiffOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), Some(&mut options))
        .context("failed to compute PR diff")
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
    merge_base: Oid,
    head: Oid,
    changed_files: &[String],
) -> Result<String> {
    let mut patch = Vec::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        patch.push(line.origin() as u8);
        patch.extend_from_slice(line.content());
        true
    })?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"attest.diff.v1\n");
    hasher.update(merge_base.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(head.to_string().as_bytes());
    hasher.update(b"\n");
    for path in changed_files {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(&patch);
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn dirty_files(ctx: &RepoContext) -> Result<Vec<String>> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let statuses = ctx.repo.statuses(Some(&mut options))?;
    let mut paths = BTreeSet::new();
    for entry in statuses.iter() {
        if let Ok(path) = entry.path() {
            paths.insert(path.replace('\\', "/"));
        }
    }
    Ok(paths.into_iter().collect())
}

fn load_directory_contracts_for_paths(
    ctx: &RepoContext,
    changed_files: &[String],
) -> Result<Vec<LoadedContract>> {
    let mut contracts = Vec::new();
    let mut seen = BTreeSet::new();
    for relative in candidate_contract_paths(changed_files) {
        if !seen.insert(relative.clone()) {
            continue;
        }
        let path = ctx.root.join(&relative);
        if !path.is_file() {
            continue;
        }
        let bytes = fs_err::read(&path).with_context(|| format!("failed to read {path}"))?;
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
//       - Identify any case where preserving an old signoff would feel misleading to a reviewer.
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
    let blocks = extract_inline_blocks(source);
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
        let Some(spec) = language.clone() else {
            bail!(
                "{path}: function-scoped inline contracts require a supported Tree-sitter language"
            );
        };
        let mut tree_parser = TsParser::new();
        tree_parser
            .set_language(&spec.language)
            .with_context(|| format!("failed to load Tree-sitter language {}", spec.name))?;
        let tree = tree_parser
            .parse(bytes, None)
            .with_context(|| format!("Tree-sitter failed to parse {path}"))?;
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
    if old.is_none() || new.is_none() {
        return true;
    }
    if change.old_path != change.new_path {
        return true;
    }
    let old = old.expect("checked old contract");
    let new = new.expect("checked new contract");
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

fn extract_inline_blocks(source: &str) -> Vec<InlineBlock> {
    let line_starts = line_start_offsets(source);
    let lines = source.lines().collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if normalize_contract_comment_line(lines[index]) != "attest: begin" {
            index += 1;
            continue;
        }
        let start_line = index + 1;
        let mut yaml_lines = Vec::new();
        index += 1;
        while index < lines.len() {
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

fn review_template(state: &PrState) -> ReviewFile {
    let mut items = Vec::new();
    for contract in &state.active_contracts {
        for claim in &contract.claims {
            items.push(ReviewItem {
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
    ReviewFile {
        kind: "attest.review.v1".to_string(),
        generated_at: Timestamp::now().to_string(),
        base_ref: state.base_ref.clone(),
        base_sha: state.base_sha.clone(),
        merge_base_sha: state.merge_base_sha.clone(),
        head_sha: state.head_sha.clone(),
        diff_digest: state.diff_digest.clone(),
        items,
    }
}

fn read_review(path: &Utf8Path) -> Result<ReviewFile> {
    let bytes =
        fs_err::read(path).with_context(|| format!("failed to read review worksheet {path}"))?;
    serde_yaml_ng::from_slice(&bytes)
        .with_context(|| format!("failed to parse review worksheet {path}"))
}

fn signoffs_from_review(state: &PrState, review: &ReviewFile) -> Result<Vec<ContractSignoff>> {
    if review.kind != "attest.review.v1" {
        bail!("review worksheet has unsupported kind `{}`", review.kind);
    }
    ensure_review_matches_state(state, review)?;

    let mut by_contract: BTreeMap<Utf8PathBuf, Vec<&ReviewItem>> = BTreeMap::new();
    for item in &review.items {
        by_contract
            .entry(item.contract_path.clone())
            .or_default()
            .push(item);
    }

    let mut signoffs = Vec::new();
    for contract in &state.active_contracts {
        let items = by_contract
            .get(&contract.path)
            .with_context(|| format!("review worksheet is missing contract {}", contract.path))?;
        let mut by_claim: BTreeMap<&str, &ReviewItem> = BTreeMap::new();
        for item in items {
            by_claim.insert(item.claim_id.as_str(), item);
        }

        let mut claim_signoffs = Vec::new();
        for claim in &contract.claims {
            let item = by_claim.get(claim.id.as_str()).with_context(|| {
                format!(
                    "review worksheet is missing claim `{}` for {}",
                    claim.id, contract.path
                )
            })?;
            if item.status != Some(ClaimStatus::True) {
                bail!(
                    "claim `{}` in {} is not true; fix the code or leave the PR unsigned",
                    claim.id,
                    contract.path
                );
            }
            if claim.evidence_required && item.evidence.iter().all(|line| line.trim().is_empty()) {
                bail!(
                    "claim `{}` in {} requires evidence before signing",
                    claim.id,
                    contract.path
                );
            }
            claim_signoffs.push(ClaimSignoff {
                id: claim.id.clone(),
                status: ClaimStatus::True,
                evidence: clean_evidence(&item.evidence),
            });
        }

        signoffs.push(ContractSignoff {
            path: contract.path.clone(),
            digest: contract.digest.clone(),
            module: contract.module.clone(),
            source: contract.source,
            target: contract.target.clone(),
            claims: claim_signoffs,
        });
    }

    Ok(signoffs)
}

fn ensure_review_matches_state(state: &PrState, review: &ReviewFile) -> Result<()> {
    if review.base_sha != state.base_sha {
        bail!(
            "review worksheet is stale: base sha is {}, current base sha is {}",
            review.base_sha,
            state.base_sha
        );
    }
    if review.head_sha != state.head_sha {
        bail!(
            "review worksheet is stale: head sha is {}, current head sha is {}",
            review.head_sha,
            state.head_sha
        );
    }
    if review.diff_digest != state.diff_digest {
        bail!(
            "review worksheet is stale: diff digest is {}, current diff digest is {}",
            review.diff_digest,
            state.diff_digest
        );
    }
    Ok(())
}

fn prompt_for_signoffs(state: &PrState) -> Result<Vec<ContractSignoff>> {
    let mut signoffs = Vec::new();
    for contract in &state.active_contracts {
        println!("\nContract: {} ({})", contract.module, contract.path);
        println!("Changed files:");
        for path in &contract.changed_files {
            println!("  - {path}");
        }

        let mut claim_signoffs = Vec::new();
        for claim in &contract.claims {
            println!("\nClaim: {}", claim.id);
            println!("{}", claim.text);
            for question in &claim.review {
                println!("  - {question}");
            }
            let status = Select::new(
                "Is this claim true after reviewing the diff?",
                vec![ClaimStatus::True, ClaimStatus::False, ClaimStatus::Unsure],
            )
            .prompt()
            .context("failed to read claim status")?;

            if status != ClaimStatus::True {
                bail!(
                    "claim `{}` was not confirmed true; fix the code or leave the PR unsigned",
                    claim.id
                );
            }

            let mut evidence = Vec::new();
            loop {
                let line = Text::new("Evidence (blank to finish):")
                    .prompt()
                    .context("failed to read evidence")?;
                if line.trim().is_empty() {
                    break;
                }
                evidence.push(line);
            }
            if claim.evidence_required && evidence.is_empty() {
                bail!("claim `{}` requires evidence before signing", claim.id);
            }

            claim_signoffs.push(ClaimSignoff {
                id: claim.id.clone(),
                status,
                evidence,
            });
        }
        signoffs.push(ContractSignoff {
            path: contract.path.clone(),
            digest: contract.digest.clone(),
            module: contract.module.clone(),
            source: contract.source,
            target: contract.target.clone(),
            claims: claim_signoffs,
        });
    }
    Ok(signoffs)
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

fn verify_state(
    ctx: &RepoContext,
    state: &PrState,
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
    let attestation = match fs_err::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Attestation>(&bytes) {
            Ok(attestation) => Some(attestation),
            Err(error) => {
                blockers.push(format!("failed to parse attestation {path}: {error}"));
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            blockers.push(format!(
                "missing PR attestation at {path}; run `attest review-pr` and `attest sign-pr --from-review`"
            ));
            None
        }
        Err(error) => {
            blockers.push(format!("failed to read attestation {path}: {error}"));
            None
        }
    };

    if let Some(attestation) = &attestation {
        verify_attestation_freshness(state, attestation, &mut blockers);
        verify_attestation_contracts(state, attestation, &mut blockers, &mut warnings);
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
        attestation: attestation.map(|attestation| AttestationSummary {
            path,
            signed_at: attestation.signed_at,
            agent: attestation.agent,
        }),
    })
}

fn verify_attestation_freshness(
    state: &PrState,
    attestation: &Attestation,
    blockers: &mut Vec<String>,
) {
    if attestation.kind != "attest.pr.v1" {
        blockers.push(format!(
            "attestation kind `{}` is unsupported",
            attestation.kind
        ));
    }
    if attestation.base_sha != state.base_sha {
        blockers.push(format!(
            "attestation base sha is stale: signed {}, current {}",
            attestation.base_sha, state.base_sha
        ));
    }
    if attestation.head_sha != state.head_sha {
        blockers.push(format!(
            "attestation head sha is stale: signed {}, current {}",
            attestation.head_sha, state.head_sha
        ));
    }
    if attestation.diff_digest != state.diff_digest {
        blockers.push(format!(
            "attestation diff digest is stale: signed {}, current {}",
            attestation.diff_digest, state.diff_digest
        ));
    }
}

fn verify_attestation_contracts(
    state: &PrState,
    attestation: &Attestation,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let mut signed_by_path: BTreeMap<&Utf8PathBuf, &ContractSignoff> = BTreeMap::new();
    for signoff in &attestation.contracts {
        signed_by_path.insert(&signoff.path, signoff);
    }

    for contract in &state.active_contracts {
        let Some(signoff) = signed_by_path.get(&contract.path) else {
            blockers.push(format!("missing signoff for contract {}", contract.path));
            continue;
        };
        if signoff.digest != contract.digest {
            blockers.push(format!(
                "contract {} changed since signing: signed {}, current {}",
                contract.path, signoff.digest, contract.digest
            ));
        }

        let mut claims_by_id: BTreeMap<&str, &ClaimSignoff> = BTreeMap::new();
        for claim in &signoff.claims {
            claims_by_id.insert(claim.id.as_str(), claim);
        }

        for claim in &contract.claims {
            let Some(claim_signoff) = claims_by_id.get(claim.id.as_str()) else {
                blockers.push(format!(
                    "missing signoff for claim `{}` in {}",
                    claim.id, contract.path
                ));
                continue;
            };
            if claim_signoff.status != ClaimStatus::True {
                blockers.push(format!(
                    "claim `{}` in {} was signed as `{}`",
                    claim.id, contract.path, claim_signoff.status
                ));
            }
            if claim.evidence_required
                && claim_signoff
                    .evidence
                    .iter()
                    .all(|line| line.trim().is_empty())
            {
                blockers.push(format!(
                    "claim `{}` in {} has no evidence",
                    claim.id, contract.path
                ));
            }
        }
    }

    for signoff in &attestation.contracts {
        if !state
            .active_contracts
            .iter()
            .any(|contract| contract.path == signoff.path)
        {
            warnings.push(format!(
                "attestation contains non-required contract {}",
                signoff.path
            ));
        }
    }
}

fn print_state(state: &PrState) {
    println!("Base: {} ({})", state.base_ref, short_sha(&state.base_sha));
    println!("Head: {}", short_sha(&state.head_sha));
    println!("Diff: {}", state.diff_digest);
    if state.changed_files.is_empty() {
        println!("Changed files: none");
    } else {
        println!("Changed files:");
        for file in &state.changed_files {
            println!("  - {file}");
        }
    }
    if !state.dirty_files.is_empty() {
        println!("Uncommitted files:");
        for file in &state.dirty_files {
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

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-' | '.'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn default_contract_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
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
    fn shell_quote_leaves_simple_refs_readable() {
        assert_eq!(shell_quote("origin/main"), "origin/main");
        assert_eq!(shell_quote("feature/test_1"), "feature/test_1");
    }

    #[test]
    fn shell_quote_handles_spaces() {
        assert_eq!(
            shell_quote("origin/main with space"),
            "'origin/main with space'"
        );
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
    fn mcp_server_exposes_attest_tools() {
        let server = AttestMcpServer::new();
        let tool_names = server
            .tool_router
            .list_all()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<BTreeSet<_>>();
        assert!(tool_names.contains("status_pr"));
        assert!(tool_names.contains("create_review"));
        assert!(tool_names.contains("sign_pr"));
        assert!(tool_names.contains("verify_pr"));
        assert!(tool_names.contains("init_contract"));
        assert!(tool_names.contains("install_hooks"));
        assert!(tool_names.contains("explain_inline"));
    }
}
