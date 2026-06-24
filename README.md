# Attest

Attest makes coding agents sign off on repo, directory, file, script, and function contracts before a PR leaves the machine.

The v0 product is intentionally narrow:

- directory contracts live in the repo as `AGENT_CONTRACT.yaml`
- contract scope comes from the directory containing that file
- inline contracts live in source comments next to the code they govern
- a root contract applies to every changed file in the repo
- a nested contract applies to changed files in that subtree
- a function contract applies only when the PR changes that function
- attestations are per PR diff, not per commit
- attestations live under `.git/attest/` and are not committed
- inline contract parse results are cached by Git blob under `.git/attest/cache/blobs/`
- agents must review claim truth before signing

## Install

```sh
cargo install --path .
```

During development:

```sh
cargo run -- status --base origin/main
```

## Codex Plugin

Attest ships as a Codex plugin from this repo. The plugin bundles:

- MCP tools through `.mcp.json`
- a Codex Stop hook through `hooks/hooks.json`
- an `attest` skill with review-before-sign instructions
- local marketplace metadata under `.agents/plugins/marketplace.json`

Install the binary first:

```sh
cargo install --path .
```

Then add this repo as a local marketplace:

```sh
codex plugin marketplace add .
```

Restart Codex, open the plugin directory, choose **Attest Local**, and install **Attest**.

The plugin MCP server starts with:

```json
{
  "attest": {
    "command": "attest",
    "args": ["mcp-server"]
  }
}
```

The bundled Stop hook is soft by default. It no-ops in repos that are not using Attest yet, and nudges the agent to review stale attestations when Attest can evaluate the PR diff.

### MCP Tools

- `status_pr`: inspect the PR diff, active contracts, and attestation freshness
- `create_review`: write `.git/attest/pr-review.yaml`
- `sign_pr`: sign reviewed claims with evidence
- `verify_pr`: verify the current PR attestation
- `init_contract`: create a starter root contract
- `install_hooks`: install project-local Codex and Git hooks
- `explain_inline`: validate an inline contract file and return its bindings

## Directory Contracts

Place a contract where its rules begin.

```text
AGENT_CONTRACT.yaml                 # applies to all changed files
crates/engine/AGENT_CONTRACT.yaml   # applies only under crates/engine/
```

If a PR changes `crates/engine/src/lib.rs`, both contracts apply.

## Directory Contract Format

```yaml
version: 1
module: engine
claims:
  - id: engine.no_test_case_heuristics
    text: I did not add logic specific to a single test fixture.
    review:
      - List each production branch added or changed.
      - Explain why each branch generalizes beyond the regression test.
      - Name the test or check that would fail if the logic were hard-coded.

  - id: engine.public_surface_intentional
    text: I did not add public API surface unless this change requires it.
```

Claim IDs should be stable. Changing a contract makes existing attestations stale.

## Inline Contracts

Put a small YAML block in comments immediately before the code it governs.

```rust
// attest: begin
// scope: function
// id: engine.resolve_move
// module: engine
// claims:
//   - id: engine.no_test_case_heuristics
//     text: resolve_move does not add branches specific to one regression fixture.
//     review:
//       - List every branch changed inside resolve_move.
//       - Explain the domain rule behind each branch.
// attest: end
pub fn resolve_move(input: MoveInput) -> Result<Move> {
    // ...
}
```

`scope: file` and `scope: script` apply when the file changes. `scope: function` binds to the next function or method using Tree-sitter. Attest compares changed lines against both the old and new function spans, so edits, moves, renames, deletions, and contract edits are caught conservatively.

Check a block before relying on it:

```sh
attest inline explain src/engine.rs
```

The output shows the contract id, the bound function or file, and the line range that activates it.

## Agent Flow

```sh
attest status --base origin/main
attest review-pr --base origin/main
```

The review worksheet is written to:

```text
.git/attest/pr-review.yaml
```

The agent must inspect the diff and fill every active claim:

```yaml
status: true
evidence:
  - Changed branch is keyed on AST node kind, not fixture filename.
```

Then sign and verify:

```sh
attest sign-pr --base origin/main --from-review
attest verify-pr --base origin/main
```

`sign-pr` refuses missing evidence, `false`, and `unsure`.

## Hooks

Install both hooks:

```sh
attest install-hooks --base origin/main
```

The Codex Stop hook nudges the agent before it finishes a stale turn. It does not say "just sign." It tells the agent to inspect the contracts and changed diff first.

The Git pre-push hook is the hard boundary:

```sh
attest verify-pr --base origin/main
```

## Commands

```sh
attest init
attest status --base origin/main
attest review-pr --base origin/main
attest sign-pr --base origin/main --from-review
attest verify-pr --base origin/main
attest codex-stop-hook --base origin/main
attest mcp-server
attest install-hooks --base origin/main
attest inline check src/engine.rs
attest inline explain src/engine.rs
attest schema contract
attest schema review
attest schema attestation
```

## Dependency Choices

Attest uses existing crates for the machinery that should not be hand-rolled:

- `git2` for merge-base and tree diff inspection
- `clap` for the CLI
- `serde`, `serde_json`, and `serde_yaml_ng` for data formats
- `tree-sitter` plus Rust, Python, JavaScript, and TypeScript grammars for inline function binding
- `blake3` for stable diff and contract digests
- `inquire` for interactive claim review
- `schemars` for JSON schemas
- `rmcp` and `tokio` for the stdio MCP server
- `camino` and `fs-err` for path and filesystem ergonomics
- `jiff` for timestamps
