# Attest

Attest is a pre-commit signoff tool for coding agents.

It blocks a commit until the agent reviews the contracts that apply to the staged diff and fills an evidence-backed attestation. Use it for judgment calls: abstraction boundaries, design taste, product expectations, and other claims a unit test cannot fully decide.

Attest is not a replacement for CI. Formatters, linters, tests, schema checks, and grep-able rules should stay in normal automation. Attest is for the review pressure you want before an agent creates a commit.

## How It Feels

```sh
git add .
git commit
```

The hook blocks and writes:

```text
.git/attest/pending-attestation.yaml
```

The agent follows the instructions at the top of that file, sets every relevant claim to `true`, adds evidence from the staged diff, sets `signed_at`, and retries:

```sh
git commit
```

No PR base. No branch stack logic. Attest signs the staged commit Git is about to create.

## Product Shape

- Attest verifies `HEAD -> index`, not a PR, branch, or working tree.
- Directory contracts live in the repo as `AGENT_CONTRACT.yaml`.
- A root contract applies to every staged file in the repo.
- A nested contract applies to staged files in that subtree.
- Inline contracts live in source comments next to the code they govern.
- A function contract applies only when the staged diff touches that function.
- The pending attestation lives under `.git/attest/` and is not committed.
- The agent must review claim truth and provide evidence before signing.
- If the staged diff changes, the pending attestation becomes stale.

## Quick Start

```sh
cargo install --path .
attest install-hooks
```

Make a change and try to commit:

```sh
git add README.md
git commit
```

The pre-commit hook inspects the staged diff, writes the pending attestation, and blocks:

```text
You need to sign these Attest contracts before committing:

- AGENT_CONTRACT.yaml
  - repo.design_intentional

Attestation draft written to:
  .git/attest/pending-attestation.yaml

Follow the instructions at the top of that file, then run:
  git commit
```

The generated YAML starts with the required procedure:

```yaml
# Attest commit signoff
#
# This file blocks the current git commit until every required claim is reviewed.
#
# Required procedure:
# 1. Inspect the staged diff for this commit.
# 2. Read each contract and each changed file listed below.
# 3. For each claim, set status to true only if the claim is actually satisfied.
# 4. Add concrete evidence from the diff for every true claim.
# 5. If any claim is false or unsure, do not sign. Report the blocker.
# 6. Set signed_at after reviewing every claim.
# 7. Run git commit again.
```

It also gives the agent the fields to fill:

```yaml
signoff:
  agent_kind: codex
  agent_session: null
  signed_at: null
```

After review, the agent fills the same file:

```yaml
signoff:
  agent_kind: codex
  agent_session: null
  signed_at: 2026-06-24T12:00:00Z

items:
  - contract_path: AGENT_CONTRACT.yaml
    claim_id: repo.design_intentional
    status: true
    evidence:
      - The staged diff keeps routing policy in the router module.
```

Then the agent runs `git commit` again. Attest verifies the staged tree, staged diff digest, contract digests, true statuses, evidence, and `signed_at`.

If the staged diff changes, Attest refreshes the draft. If the same staged diff is still present, Attest leaves the draft alone so existing evidence is not erased.

## Hook Setup

Install the native Git hook:

```sh
attest install-hooks
```

This writes `.git/hooks/pre-commit`:

```sh
attest pre-commit-hook
```

For `pre-commit`, add Attest to `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/Bradley-Butcher/Attest
    rev: v0.1.0
    hooks:
      - id: attest
```

For local development without installing from GitHub:

```yaml
repos:
  - repo: local
    hooks:
      - id: attest
        name: Attest commit signoff
        entry: attest pre-commit-hook
        language: system
        pass_filenames: false
        always_run: true
        require_serial: true
        stages: [pre-commit]
```

`prek` uses the same `.pre-commit-config.yaml` shape, so the same hook entry works there too.

## Contract Design

Write contracts for judgment, not automation.

Good Attest claims ask the agent to explain:

- why an abstraction boundary still holds
- why a new public surface belongs in this change
- why a behavior is product-correct, not just test-passing
- why a fixture did not become production policy

Poor Attest claims ask the agent to restate:

- code formatting
- lint rules
- unit test results
- schema validity
- facts a script can check deterministically

## Directory Contracts

Place a contract where its rules begin.

```text
AGENT_CONTRACT.yaml                 # applies to all staged files
crates/engine/AGENT_CONTRACT.yaml   # applies only under crates/engine/
```

If a commit stages `crates/engine/src/lib.rs`, both contracts apply.

## Directory Contract Format

```yaml
version: 1
module: engine
claims:
  - id: engine.no_fixture_specific_logic
    text: I did not add logic specific to a single fixture.
    review:
      - List each production branch added or changed.
      - Explain why each branch generalizes beyond the regression fixture.

  - id: engine.public_surface_intentional
    text: I did not add public API surface unless this change requires it.
```

Claim IDs should be stable. Changing a contract makes the pending attestation stale.

## Inline Contracts

Put a small YAML block in comments immediately before the code it governs.

```rust
// attest: begin
// scope: function
// id: engine.resolve_move
// module: engine
// claims:
//   - id: engine.no_fixture_specific_logic
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

## Commands

```sh
attest init
attest status
attest review
attest review --print
attest verify
attest pre-commit-hook
attest install-hooks
attest inline check src/engine.rs
attest inline explain src/engine.rs
attest schema contract
attest schema attestation
```

## Dependency Choices

Attest uses existing crates for the machinery that should not be hand-rolled:

- `git2` for staged tree and staged diff inspection
- `clap` for the CLI
- `serde`, `serde_json`, and `serde_yaml_ng` for data formats
- `tree-sitter` plus Rust, Python, JavaScript, and TypeScript grammars for inline function binding
- `blake3` for stable staged diff and contract digests
- `schemars` for JSON schemas
- `camino` and `fs-err` for path and filesystem ergonomics
- `jiff` for timestamps
