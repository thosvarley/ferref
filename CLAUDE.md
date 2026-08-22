# ferref

Rust CLI reference manager over a plain SQLite file. Design, phase plan, and
principles: `DESIGN.md`. Work proceeds one phase at a time, in order.

## Before starting a new phase

Read the phase's section in `DESIGN.md`, then its row in **Delegation policy**
(end of `DESIGN.md`) to decide whether to hand the work to the `coder` subagent
and whether to follow up with `adversarial-reviewer`.

Subagents are defined in `.claude/agents/`. They start cold — the brief must
carry the schema details and constraints they need.

## Conventions

- No new dependencies without a reason stated in `DESIGN.md`.
- DB access is free functions taking `&Connection` — no wrapper struct, no traits.
- Every data-printing CLI command supports `--json`, from the command's first version.
- Existing tutorial-style comments in `models.rs`/`db.rs` are a learning artifact;
  don't replicate that density in new code, don't strip it from code you aren't
  otherwise touching.
- Non-trivial logic leaves one runnable check behind — an assert-based `#[test]`,
  not a suite.
