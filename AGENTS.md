# Lan Mouse Agent Instructions

## Overview

Lan Mouse is an open-source Software KVM sharing mouse/keyboard input across local networks. The Rust workspace combines a GTK frontend, CLI/daemon mode, and multi-OS capture/emulation backends for Linux, Windows, and macOS.

## Core principles

- **Scope discipline.** Only implement what was requested; describe follow-up work instead of absorbing it.
- **Clarify OS behavior.** Ask when requirements touch OS-specific capture/emulation (they differ significantly).
- **Docs stay current.** Update [README.md](README.md) or [DOC.md](DOC.md) when touching public APIs or platform support.
- **Rust idioms.** Use `Result`/`Option`, `thiserror` for errors, descriptive logs, and concise comments for non-obvious invariants.

## Design approach

- Start with the data: its ownership, lifetime, access patterns, and the transformations required by the approved task.
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

## Issue planning and approval

Track requested new features and confirmed bugs as GitHub issues in `NickReardon/lan-mouse`. Check for existing issues before creating one, link implementation work to its issue, and include evidence or reproduction steps for bugs. Keep unverified concerns clearly marked as investigation tasks rather than confirmed bugs. Do not file fork-specific work in the upstream repository unless requested.

- Track features, bugs, investigations, maintenance, and documentation tasks in issues. Each issue has one responsibility. Use parent issues for larger initiatives, with independently scoped sub-issues, blockers, shared interfaces, and an integration owner.
- Keep the revisioned plan, scope, acceptance criteria, dependencies, and validation on the issue. Clarify ambiguous requirements, especially OS-specific capture/emulation behavior, during planning.
- **Do not claim an issue, create an implementation branch, or start implementation until a human approves the current plan revision.** Read-only investigation and issue planning may happen before approval.
- Record human approval on the issue as `Approved revision N`. Explicit approval in the owner's chat may be recorded by the agent with its source clearly identified; never present an agent's own comment, label, or template checkbox as human approval.
- Material changes to scope, acceptance criteria, or agreed interfaces need a revised plan and renewed approval. Routine implementation choices within approved scope do not. Parent approval does not implicitly approve unspecified child plans.

## Ownership and worktrees

- After approval, read current claims and dependencies. Claim the issue with a unique agent/session ID, machine, approved revision, scope, branch, worktree, and next checkpoint. Re-read the issue to check for conflicting claims before writing code; GitHub comments are advisory, not an atomic lock. Resolve collisions before work begins.
- One agent writes to an issue branch at a time. Separate approved issues may run in parallel; coordinate shared interfaces first. Reviewers may inspect another branch without writing to it.
- Silence or an old timestamp does not release ownership. A stale claim requires explicit release or human-authorized takeover recorded on the issue.
- Work in a separate Git worktree under the primary clone's ignored `.worktrees/` directory, using a branch such as `codex/<issue>-<slug>`. Check `git worktree list` before creating or reusing a checkout. Do not implement in the primary checkout, switch another agent's branch, or edit another agent's worktree.
- Run commands from the task worktree root. Worktrees share Git metadata; do not change shared repository settings, clean shared files, or rewrite shared history without coordination.

## Commits, draft PRs, and handoffs

- Make frequent, small commits, each covering one coherent change. Push after each meaningful checkpoint and before pausing, stopping, or handing off. Keep all needed WIP recoverable from the remote branch; do not rely on a local stash or chat history. Never include secrets or machine-specific artifacts.
- Approved implementation includes routine branch commits/pushes, opening a draft PR, and progress comments. Do not ask again for each checkpoint. Respect installed commit hooks; do not bypass them to publish WIP.
- Open a draft PR after the first implementation push and link it to the issue. Keep its description current. Issues hold plans, approvals, dependencies, and ownership; PRs hold the implementation summary, diff, progress, and review.
- After a meaningful part is completed, commit/push, then comment on the draft PR with the SHA, completed behavior, checks/results, and remaining work or blockers. Update the issue at major milestones, scope/approval changes, ownership changes, and handoff. Link records rather than duplicating detailed status; avoid per-command or unchanged-status comments.
- WIP commits are allowed: explicitly record incomplete behavior, failures, and untested changes. Add proportional tests, run affected-crate tests, and run `cargo fmt` and `cargo clippy --workspace --all-targets --all-features` for Rust changes. Documentation-only work uses relevant document/template checks. Report existing failures and unavailable checks honestly.
- Before declaring a PR ready, meet approved acceptance criteria, pass required checks, document limitations, and push all implementation commits. Keep it draft while required target-platform verification is outstanding unless a human approves a documented exception.
- For cross-machine handoff, push first and record PR URL, branch, full pushed SHA, approved revision, completed/remaining work, checks, blockers, and ownership release/transfer on the issue. The receiver fetches, verifies approval and SHA, confirms sole ownership, and resumes in its own local worktree. No concurrent writers, force pushes, or disruptive history rewrites.
- Before removing a worktree, verify all needed commits are pushed and preserve needed uncommitted and ignored files. Record completion/handoff and release ownership first. Never force-remove a dirty worktree or discard another agent's work.
- Merging, publishing releases, and creating/pushing version tags require separate human authorization. GitHub builds run only on pushed `v*` tags; do not change that policy or create tags to validate ordinary work. Development checks run locally.

See [CONTRIBUTING.md](CONTRIBUTING.md) for commands and plan approval, claim, progress, and handoff templates. Other agent entry points must refer to this file rather than duplicate its policy.
