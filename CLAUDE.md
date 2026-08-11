# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## White-room policy — no exceptions

All work on this repo is white-room (clean-room). Never read, quote, consult, or reproduce GNU make's source code (or any other GPL make implementation's source) for any purpose — not for reference, not to check an edge case, not in a subagent. Permitted sources of behavior: the GNU make manual, POSIX, and black-box observation of the `make` binary (running it and comparing output, as tests/differential.rs does). If a question can only be answered by reading GNU source, stop and say so instead.

Propagate this rule verbatim into the prompt of every subagent dispatched to work on this repo.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- All tests: `cargo test` — the differential suite requires GNU `make` on PATH
- One suite: `cargo test --test semantics` (also: `differential`, `execution`, `refusals`, `graph`)
- One test: `cargo test --test refusals <substring_of_test_name>`
- Lint: `cargo clippy --all-targets`

Zero dependencies is a deliberate policy (see the comment in Cargo.toml) — do not add crates; hand-roll instead.

## Architecture

rsmake is both a `[[bin]]` and a `[lib]`; the pipeline is a library so the test harness can drive it in-process. The dialect is deliberately closed: a construct outside it is an error naming the construct, never a silent skip (see the `REJECTED_*` tables in parse.rs/expand.rs — unimplemented GNU constructs are refused by name, not ignored).

Pipeline, in order:

- `src/lib.rs::parse_args_with` — hand-rolled CLI parsing (clustered flags, `VAR=value` positionals, bare goals), returning `Request::{Run,Help,Version}` so it's testable in-process; `src/main.rs` only prints and exits.
- `src/parse.rs` — line-oriented makefile parser: continuations, conditionals (tracked even in dead branches), `define`/`endef` capture, `include` (cycle-guarded), rule-vs-assignment split, suffix-rule → pattern-rule rewrite, special targets, built-in rule installation.
- `src/expand.rs` — `$()` expansion: the function table (`arity` gates what's a function), pattern matching/`%` stems, glob matching for `$(wildcard)`, `$(shell)`, cycle/depth guard. Delimiter-aware argument splitting is threaded from `expand_scan` (which knows whether `(` or `{` opened the reference).
- `src/graph.rs` — dependency discovery from goals: explicit/pattern/implicit rule search, VPATH resolution (expanded once per discovery pass, memoized), staleness (mtime comparison, order-only prereqs excluded), automatic variables (`$@ $< $^ $?` …).
- `src/run.rs` — scheduler and recipe execution: counter-based readiness with worker threads for `-j`, per-job output capture for atomicity, child environment (`MAKEFLAGS`/`MAKELEVEL` bookkeeping), echo/silent/ignore semantics per line prefix (`@ - +`).

Variable semantics live in `lib.rs`: `Vars` implements origin-precedence ranking (default < environment < file < command line < override/automatic; `-e` promotes environment above file), `Flavor` distinguishes recursive from simple assignment, and target-specific variables are pushed/popped around each rule instance.

## Testing model

The behavioral contract is GNU make; conformance is proven, not assumed:

- `tests/differential.rs` — the oracle. Every directory under `tests/corpus/` containing a `Makefile` is run through both real `make` and rsmake (args default to `-n`; an `ARGS` file in the entry overrides them, whitespace-separated) and stdout + exit code are diffed. Add a corpus entry for any behavior GNU exhibits that rsmake must match; verify against real GNU output, never against what you expect GNU to do.
- `tests/semantics.rs` — in-process expansion/parsing assertions via the library API.
- `tests/execution.rs` — real builds in temp dirs (parallelism, ordering, file effects).
- `tests/refusals.rs` — the closed-dialect error paths: every rejection must name the construct.

When fixing a divergence from GNU, reproduce it against the real binary first, then encode it as a corpus entry or test so the suite ratchets.
