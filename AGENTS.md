# AGENTS.md

Instructions for coding agents working in this repo. `CLAUDE.md` only imports this file;
always edit here.

## Project

`clipbroad` syncs the clipboard across the owner's machines (Linux/Hyprland, macOS, Windows) with
a P2P daemon in Rust, without a server. **Read `docs/DESIGN.md` before any non-trivial change**: it
holds the architecture decisions, the phased plan and the known risks.

The project owner supervises by results and does not read Rust in detail. Because of that:
- Every change must leave verifiable evidence: new tests, command output, or explicit manual test
  steps.
- Explain in the PR **what** changed and **how it was verified**, in plain language.
- If something could not be verified (for example, real behavior on macOS or Windows), say so
  explicitly.

## Structure

```
crates/core       # content types, protocol, anti-echo, filters. No OS code.
crates/clipboard  # Clipboard trait + per-OS backends + in-memory fake for tests
crates/net        # iroh transport, pairing, broadcast
crates/config     # TOML config, identity (secret key) and per-OS paths
crates/daemon     # `clipbroad` binary (CLI + orchestration)
docs/DESIGN.md    # decisions and plan
```

Layer rules: `core` depends on nothing from the OS or the network. Everything that touches the OS lives in
`clipboard` behind `#[cfg(target_os = ...)]`. `daemon` only joins the pieces.

## Commands

Install the tools with `mise install` (see `mise.toml`).

```sh
cargo fmt --all                                         # format
cargo clippy --workspace --all-targets -- -D warnings   # lints (warnings = error)
cargo nextest run --workspace --no-tests=pass           # tests
cargo test --workspace --doc                            # doctests (nextest does not run them)
cargo deny check                                        # advisories, licenses, sources
cargo machete                                           # unused dependencies
cargo run -p clipbroad -- --help                        # run the binary
```

**Before calling a task done, all of these commands must pass.** CI runs the same on
Linux, macOS and Windows.

## Code conventions

- Everything is **in English**: code, identifiers, comments, commit messages, PRs and the documentation
  in `docs/`.
- Edition 2024. Workspace lints in `Cargo.toml` (`clippy::pedantic`, `unwrap_used`, `panic`,
  `unsafe_code = "deny"`). Do not silence lints with `#[allow]` without a comment that explains why.
- Errors: `thiserror` in the library crates, `anyhow` in `daemon`. No `unwrap()` outside tests;
  `expect()` only for invariants that indicate a bug, with a message that says which one.
- Async with `tokio`. Logging with `tracing`. No `println!` for diagnostics.
- `unsafe`: only if an OS API has no safe binding, with a local `#[allow(unsafe_code)]` and a
  `// SAFETY:` comment that justifies each block.
- Platform code always behind the `Clipboard` trait; test the logic with the fake.

## Dependencies

- Before adding a crate: check the current version (`cargo info <crate>`), that it is maintained and
  that its license passes `cargo deny check`. Justify the dependency in the PR.
- Shared versions go in `[workspace.dependencies]`; crates inherit them with `.workspace = true`.
- Prefer the crates already chosen in `docs/DESIGN.md` (iroh, wl-clipboard-rs, arboard, ...).

## Rust guide

The `rust-skills` skill (`.claude/skills/rust-skills/SKILL.md`, rules in `rules/`) has good
Rust practices. Agents without skill support can read the files directly. Priorities:
1. Correctness and clarity first: ownership, error handling, async, API.
2. The performance and memory rules (`mem-*`, `opt-*`, `perf-*`: SmallVec, arenas, etc.) **only** in
   hot, measured code. Do not add dependencies because those rules suggest it without justifying it.

Reference for deeper reviews: [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines).

## Workflow

- One branch and one PR per unit of work, ideally a phase or sub-phase of `docs/DESIGN.md`.
- Commits with [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, ...).
- A PR is merged only with green CI on all three OSes.
- If a design decision changes, update `docs/DESIGN.md` in the same PR.
- Do not push to `main` directly.
