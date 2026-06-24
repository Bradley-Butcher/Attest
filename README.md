# Attest

Attest makes coding agents sign the staged commit before Git creates it.

Use Attest for judgment calls about design, abstraction boundaries, and product expectations. Keep deterministic checks in normal CI.

The v0 product is intentionally narrow:

- directory contracts live in the repo as `AGENT_CONTRACT.yaml`
- contract scope comes from the directory containing that file
- inline contracts live in source comments next to the code they govern
- a root contract applies to every staged file in the repo
- a nested contract applies to staged files in that subtree
- a function contract applies only when the staged diff touches that function
- attestations are per staged commit, not per PR or branch
- the pending attestation lives at `.git/attest/pending-attestation.yaml`
- inline contract parse results are cached by Git blob under `.git/attest/cache/blobs/`
- agents must review claim truth and provide evidence before signing

## Install

```sh
cargo install --path .
```

During development:

```sh
cargo run -- status
```

## Commit Flow

Stage the intended commit:

```sh
git add .
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

The generated YAML starts with the required procedure. The agent fills the same file:

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

## Hooks

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
