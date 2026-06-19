# 🤖 AGENT DIRECTIVES: JMAP-Matrix Bridge

**Context:** This is a Rust-based Matrix Application Service bridging JMAP email to Matrix rooms.
**Prime Directive:** You are an autonomous AI entity operating in this repository. You must adhere strictly to the workflows, architectural boundaries, and toolchains defined below. Do not guess commands, and do not ignore environmental constraints.

**Before you start:** read [`CONTEXT.md`](CONTEXT.md) for the domain vocabulary (ghost, puppet, bot, thread, room, space, submission, backfill) and [`docs/adr/`](docs/adr/) for the decisions of record. Use that vocabulary in code, tests, and issues; if your change contradicts an ADR, surface it rather than silently overriding.

---

## 1. The Environment: Nix Flake (STRICT BOUNDARY)

This repository is completely managed by a **Nix flake**. This means the development environment is fully declarative, reproducible, and sandboxed.

* **DO NOT** attempt to install any system packages (`apt`, `brew`, `apk`).
* **DO NOT** use `rustup` to change toolchains or install components.
* **DO NOT** use `cargo install` for global binaries.
* **All tools** (Rust compiler, `cargo-nextest`, `bacon`, `just`, etc.) and system dependencies are pre-provisioned via the `flake.nix` development shell (`nix develop` or `direnv`).
* **If a system dependency is missing:** You must modify the `flake.nix` inputs/packages. Do not attempt to work around it locally.

---

## 2. Toolchain & Workflow (Your "Hands" and "Eyes")

Do NOT run raw `cargo` or `bash` commands unless explicitly required. Your primary interface for interacting with this codebase is `just`. Run `just` at any time to list available commands.

### The Inner Loop (Writing & Verifying)
When modifying source code, choose your feedback loop based on your execution capabilities:
* **For agents with background/asynchronous execution:** Start your session by running `bacon --job check > bacon.log &`. Monitor `bacon.log` for real-time compiler feedback as you edit files. 
* **For turn-based/synchronous agents:** Run `just check` after every logical file edit. Do not proceed to testing or further edits until `just check` passes cleanly.

### The Outer Loop (Testing & Fixing)
* **Testing:** ALWAYS use `just nextest` to verify logic. It utilizes `cargo-nextest` for heavy parallelization. Do not use standard `cargo test` unless `nextest` fails due to an unknown runner incompatibility.
* **Linting:** Run `just lint` before considering a task complete. If you encounter Clippy warnings, run `just fix` to automatically resolve safe lints before manually fixing the rest.
* **Formatting:** Run `cargo fmt` before finalizing any file modifications.

---

## 3. Architectural Rules & Code Style

When generating or refactoring code, you MUST adhere to the following constraints:

### Language & Safety
* **Edition:** Rust 2024.
* **Safety:** `#![forbid(unsafe_code)]` is enforced at the workspace level. Do not attempt to write or suggest `unsafe` blocks.
* **Lints:** We use `clippy::all`, `clippy::pedantic`, `clippy::nursery`, and `clippy::cargo`. Code must compile cleanly against these.
    * *Exception:* Test modules (`cfg(test)`) may use `#[allow(clippy::unwrap_used)]`.

### Error Handling
* **Libraries (`src/lib/` etc):** MUST use `thiserror` for structured, typed error enums.
* **Binaries (`src/main.rs` etc):** MUST use `anyhow` for rapid error propagation and context.

### Naming Conventions
* `snake_case` for functions, variables, and modules.
* `PascalCase` for types, structs, and traits.
* `SCREAMING_SNAKE_CASE` for constants and statics.

### Concurrency & State
* **Async Runtime:** `tokio` (full features). Ensure all IO and network calls are non-blocking.
* **Shared State:** Guard shared mutable state with `tokio::sync::{RwLock, Mutex}` wrapping a plain `HashMap`/`HashSet`, held briefly (clone an `Arc` out, or insert/remove, then drop the guard). These maps are per-user and cold — do NOT reach for `dashmap::DashMap`; its sync, `!Send`-across-`.await` guards earn nothing without a hot, high-contention path. See [ADR-0003](docs/adr/0003-tokio-async-locks-over-dashmap.md).
* **Persistence:** Use `sqlx` with SQLite. Do not use ORMs like Diesel or SeaORM. Queries run at **runtime** via the unchecked `sqlx::query`/`query_as` APIs — the project deliberately does **not** use the compile-time `sqlx::query!` macros, so the build needs no database, no `DATABASE_URL`, and no `sqlx-data.json`. Schema changes are SQL migrations in `migrations/`, applied at startup by `sqlx::migrate!` (`src/store/connection.rs`).
    * If you ever introduce a compile-time `sqlx::query!` macro, you must also commit the offline query cache (`cargo sqlx prepare`) so the hermetic Nix sandbox still builds — but prefer the runtime APIs already used throughout `src/store/`.

---

## 4. Testing & CI Pipeline

* **Execution:** ALWAYS run `just nextest` to verify your changes. 
* **Mocking:** When writing NEW integration tests inside the `tests/` directory, you MUST use `wiremock` to mock HTTP endpoints (JMAP or Matrix homeservers). Do not make live network requests. Your tests must be able to pass inside an offline Nix sandbox.
* **CI / Release Builds:** The final source of truth for a successful build is the Nix sandbox. CI (`.github/workflows/ci.yml`) runs exactly one gate on every PR/push across x86_64 + aarch64: **`nix flake check`**, which builds the package and runs clippy, `rustfmt --check`, the `cargo-nextest` suite, and (x86_64) the VM round-trip test. Run `just lint && just nextest` (or `nix flake check`) locally before pushing — `main` must stay green, formatting included.

---

## 5. Releases

Releases are automated with **release-plz** (on-demand cadence). Conventional Commit subjects (`feat:`/`fix:`/`refactor:`/…) drive everything — write them accordingly.

* Every push to `main` updates a **"release vX.Y.Z" PR** (version bump + `CHANGELOG.md`). **Cutting a release = merging that PR**, which tags `vX.Y.Z` and creates the GitHub Release. Pre-1.0: breaking → `0.x.0`, feat/fix → `0.x.y`.
* The tag triggers `release-artifacts.yml`: static (musl) binaries per arch on the Release, plus a multi-arch OCI image on `ghcr.io` (built from `nix build .#static` / `.#dockerImage`). No crates.io publish.
* Full rationale + the one-time secret/setup checklist (Cachix `CACHIX_AUTH_TOKEN`, `RELEASE_PLZ_TOKEN`, ghcr visibility) are in [ADR-0008](docs/adr/0008-ci-and-release-flow.md).

---

## Agent skills

### Issue tracker

Issues are tracked as GitHub issues in `palebluebytes/jmap-matrix-bridge` via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
