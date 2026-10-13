# Lan Mouse Agent Instructions

## Overview

Lan Mouse is an open-source Software KVM sharing mouse/keyboard input across local networks. The Rust workspace combines a GTK frontend, CLI/daemon mode, and multi-OS capture/emulation backends for Linux, Windows, and macOS.

## Core principles

- **Scope discipline.** Only implement what was requested; describe follow-up work instead of absorbing it.
- **Clarify OS behavior.** Ask when requirements touch OS-specific capture/emulation (they differ significantly).
- **Docs stay current.** Update [README.md](README.md) or [DOC.md](DOC.md) when touching public APIs or platform support.
- **Rust idioms.** Use `Result`/`Option`, `thiserror` for errors, descriptive logs, and concise comments for non-obvious invariants.

## Design approach

- Start with the data: its ownership, lifetime, access patterns, and the transformations required by the task.
- Prefer procedural functions with explicit inputs, outputs, and control flow. Separate data representation from behavior where useful; minimize hidden mutable state.
- Prefer concrete types and simple composition. Add abstraction layers or traits when they solve a demonstrated need; retain useful existing backend contracts, enums, and Rust safety idioms.
- Let the workload guide data layout. Use contiguous storage or batching when justified; do not mechanically require structure-of-arrays or redesign the event/stream pipeline.
- Measure performance-sensitive changes against a representative workload before adding layout complexity. Record workload and before/after results; speculative optimization or architectural rewrites need separate issues and approval.

## Terminology

- **Client:** A remote machine that can receive or send input events. Each client is either _active_ (receiving events) or _inactive_ (can send events back). This mutual exclusion prevents feedback loops.
- **Backend:** OS-specific implementation for capture or emulation (e.g., libei, layer-shell, wlroots, X11, Windows, macOS).
- **Handle:** A per-client identifier used to route events and track state (pressed keys, position).

## Architecture

**Pipeline:** `input-capture` → `lan-mouse-ipc` → `input-emulation`

- **input-capture:** Reads OS events into a `Stream<CaptureEvent>`. Backends tried in priority order (libei → layer-shell → X11 → fallback). Tracks `pressed_keys` to avoid stuck modifiers. `position_map` queues events when multiple clients share a screen edge.
- **input-emulation:** Replays events via the `Emulation` trait (`consume`, `create`, `destroy`, `terminate`). Maintains `pressed_keys` and releases them on disconnect.
- **lan-mouse-ipc / lan-mouse-proto:** Protocol glue and serialization. Events are UDP; connection requests are TCP on the same port. Version bumps required when serialization changes.
- **input-event:** Shared scancode enums and abstract event types—extend here, don't duplicate translations.

## Feature & cfg discipline

- Feature flags live in root `Cargo.toml`. Gate OS-specific modules with the configs exported in build.rs (e.g., `cfg(layer_shell)`).
- Prefer module-level gating over per-function cfgs to avoid empty stubs.
- New backends: add feature in `Cargo.toml`, create gated module, log backend selection.

## Async patterns

- Tokio runtime with `futures` streams and `async_trait`. Model new flows as streams or async methods.
- Avoid blocking; use `spawn_blocking` if needed. Prefer existing single-threaded stream handling.
- `InputCapture` implements `Stream` and manually pumps backends—don't short-circuit this logic.

## Commands

```sh
cargo build --workspace                                    # full build
cargo build -p <crate>                                     # single crate
cargo test --workspace                                     # all tests
cargo fmt && cargo clippy --workspace --all-targets --all-features  # lint
LAN_MOUSE_LOG_LEVEL=debug cargo run                        # debug logging
```

Run from repo root—no `cd` in scripts.

## Testing

- Unit tests for utilities; integration tests for protocol behavior.
- OS-specific backends: test via GTK/CLI on target OS or document manual verification.
- Dummy backend exercises pipeline without real dependencies.
- Verify `terminate()` releases keys on unexpected disconnect.

## Workflow

The owner tests by switching branches in a single checkout and running `cargo build`/`cargo run`. Keep it that way.

- **One checkout.** Work on a normal branch in the repository root. Do not create Git worktrees, separate clones, or custom `--target-dir` locations unless the owner asks.
- **Branches.** Branch from an up-to-date `main` with a short descriptive name. Check `git status` before switching branches; never switch with uncommitted changes, and never discard or stash the owner's changes without asking. Commit work in progress instead.
- **Issues.** Use GitHub issues in `NickReardon/lan-mouse` for work worth tracking; link them from commits or PRs. For confirmed bugs, include reproduction steps; label unverified concerns as investigations. Do not file fork-specific work upstream.
- **Ask first** for large or ambiguous changes, especially OS-specific capture/emulation behavior. Small, clearly requested changes need no separate plan.
- **Commits.** Make small, coherent commits and push the branch so work is not local-only. Respect commit hooks. Never commit secrets or machine-specific artifacts.
- **Checks.** For Rust changes, run affected-crate tests, `cargo fmt`, and `cargo clippy --workspace --all-targets --all-features`. Document manual target-OS verification for capture/emulation changes. Report failures and skipped checks honestly.
- **PRs.** Open a PR against `main` describing the change and how it was tested. Keep it draft while required target-OS verification is outstanding.
- **Needs explicit approval:** merging, force-pushing or rewriting pushed history, publishing releases, and creating or pushing tags. GitHub builds run only on pushed `v*` tags; development checks run locally.

Other agent entry points must refer to this file rather than duplicate its policy.
