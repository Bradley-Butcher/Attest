---
name: attest
description: Use Attest to review directory and inline PR contracts, create evidence-backed signoffs, verify PR attestations, and install Attest hooks. Use when a repository contains AGENT_CONTRACT.yaml files, inline attest comment blocks, or the user asks about Attest contract signoffs.
---

# Attest

Attest makes coding agents review and sign directory and inline PR contracts.

Use the MCP tools when available. Fall back to the `attest` CLI when the MCP server is not loaded.

## Rules

- Do not sign mechanically.
- Run status before signing.
- Inspect the current diff and every active contract claim.
- For inline function contracts, inspect the bound function body and the changed lines that activated it.
- Mark a claim `true` only when the claim is actually true.
- Use `false` or `unsure` when the code does not satisfy the claim or the evidence is not clear.
- Fix the code or report a blocker when any required claim is false or unsure.
- Include concrete evidence for every true claim.

## MCP Flow

1. Call `status_pr`.
2. Call `create_review`.
3. Inspect the changed files and active claims.
4. Call `explain_inline` when an inline contract binding is unclear.
5. Call `sign_pr` only with claim reviews that are true and evidence-backed.
6. Call `verify_pr`.

## CLI Flow

```sh
attest status --base origin/main
attest review-pr --base origin/main
attest sign-pr --base origin/main --from-review
attest verify-pr --base origin/main
```

## Contract Scope

Directory contract scope is derived from file location.

```text
AGENT_CONTRACT.yaml                 # applies to all changed files
crates/engine/AGENT_CONTRACT.yaml   # applies only under crates/engine/
```

If a PR changes `crates/engine/src/lib.rs`, the agent must satisfy both the root contract and the `crates/engine` contract.

Inline contracts live in source comments. `scope: file` and `scope: script` apply when that file changes. `scope: function` binds to the next function or method and applies when changed lines touch the old or new bound function span.

```rust
// attest: begin
// scope: function
// id: engine.resolve_move
// module: engine
// claims:
//   - id: engine.no_test_case_heuristics
//     text: resolve_move does not add branches specific to one regression fixture.
// attest: end
pub fn resolve_move(input: MoveInput) -> Result<Move> {
    // ...
}
```

Use this when placement is unclear:

```sh
attest inline explain src/engine.rs
```
