# Contributing with agents

[AGENTS.md](AGENTS.md) is the shared policy for all agents. [CLAUDE.md](CLAUDE.md)
imports it for Claude Code. This guide supplies the procedure and reusable
records; plans and approval live in GitHub issues, not a separate local plan file.

## Plan one responsibility

Search [the fork's issues](https://github.com/NickReardon/lan-mouse/issues) before
creating a feature, bug, investigation, maintenance, or documentation task. Use
the **Focused task or initiative** issue form. Confirmed bugs need evidence and
reproduction steps; unverified concerns are investigations.

Each implementable issue has one outcome and a revisioned plan containing scope,
non-goals, proposed changes, acceptance criteria, dependencies, and validation.
Start with the data, ownership, access patterns, and transformations. Prefer
procedural functions and explicit control flow; justify abstraction and layout
complexity with a concrete need or measured workload.

Split a larger initiative into sub-issues with independent outcomes. Use GitHub
sub-issues where available and link the parent/children in their descriptions.
The parent coordinates integration; each child identifies blockers, shared
interfaces, and its own approval. Agree on shared interfaces before parallel
implementation and name an integration owner. File unrelated discoveries as
separate issues instead of expanding the task.

## Approve, then claim

Before claiming an issue or creating an implementation branch, obtain explicit
human approval for its current plan revision. Read-only investigation and issue
planning may happen beforehand. A label, assignment, agent comment, or submitted
form is not evidence of human approval.

A human can post:

```text
Approved revision <N> of issue #<issue>.
Scope and acceptance criteria: as recorded in that revision.
Exceptions or conditions: <none, or explicit constraints>.
```

If approval is given directly in the owner's chat, record the instruction and
its source on the issue, identifying it as an agent's record of human approval.
Do not impersonate the human approver. Preserve approved revisions before
replacing the plan with a materially changed revision. Scope, acceptance, or
agreed interface changes require renewed approval; routine implementation
choices within approved scope do not.

Read current issue claims and blockers, then post a claim:

```text
Claim: <unique agent/session ID>
Machine: <hostname / OS>
Approval: revision <N>, <approval comment or human chat reference>
Scope: <approved responsibility>
Branch: codex/<issue>-<slug>
Worktree: <absolute local clone>/.worktrees/<issue>-<slug>
Dependencies / shared interfaces: <links and agreed contracts, or none>
Next checkpoint: <small deliverable>
```

Re-read the issue after posting. Comments are an advisory coordination mechanism,
not an atomic lock: if claims collide, resolve ownership before creating the
branch or implementing. One agent writes to an issue branch at a time. Agents
may work on separate approved issues concurrently or review without writing.
Assignments identify GitHub users; the claim's session ID identifies the agent.
Silence does not release a claim. A stale claim needs an explicit release or a
human-authorized takeover recorded on the issue.

## Create an isolated worktree

The primary checkout is for coordination. Implementation happens in a separate
worktree under its ignored `.worktrees/` folder. Run these Git setup commands from
the primary clone root, substituting the real approved issue number and slug for
the illustrative `42-short-description`:

```sh
git status --short
git worktree list
git fetch origin
git worktree add -b codex/42-short-description .worktrees/42-short-description origin/main
```

Open the new worktree as the agent's workspace. Run its edits, build commands,
and tests from that worktree root. Do not switch or modify another agent's
checkout, force an already-checked-out branch, or change shared Git settings.
Uncommitted primary-checkout files do not automatically appear in a worktree;
preserve them and coordinate any needed transfer rather than silently copying
unrelated work. Keep secrets, local credentials, and machine-specific artifacts
out of commits.

## Commit, push, and open a draft PR

Make small commits covering coherent changes. Review and stage specific paths;
avoid sweeping another agent's changes into a commit. From the task worktree:

```sh
git diff --check
git diff
# Stage the reviewed files, then commit the checkpoint with a reference to its issue.
git push -u origin codex/42-short-description
gh pr create --repo NickReardon/lan-mouse --base main --head codex/42-short-description --draft
```

Open the draft PR after the first implementation push and link its issue and
approved revision. Keep the description accurate as the result evolves. Use the
PR template for the current behavior, validation, and outstanding work.

Push after each meaningful checkpoint and before pausing, stopping, or handing
off. Approved implementation authorizes these branch commits/pushes and routine
draft-PR updates without asking again each time. WIP is allowed when clearly
described; respect installed commit hooks and report a blocker if they prevent a
checkpoint. Never bypass hooks, force-push, or rewrite shared WIP history merely
to make a checkpoint. If a push fails, report that the checkpoint is local-only;
do not claim it is recoverable from another machine.

Post a concise PR progress comment after meaningful completed work:

```text
Checkpoint: <full pushed commit SHA>
Completed: <behavior or approved plan step now implemented>
Checks: <commands and results; identify untested work or existing failures>
Remaining / blockers: <next work, dependencies, or none>
```

The issue holds the approved plan, acceptance criteria, dependencies, and
ownership. The PR holds the implementation diff, current summary, detailed
progress, and review. Update the issue at major milestones, scope/approval
changes, ownership changes, and handoff, linking the PR rather than duplicating
each progress comment. Do not post per-command or unchanged-status updates.

## Validate locally and prepare review

Install the platform dependencies described in [README.md](README.md#development).
Use proportional tests for affected behavior. For Rust changes, run affected
crate tests, `cargo fmt`, and `cargo clippy --workspace --all-targets --all-features`.
Document target-OS manual verification for capture/emulation changes. Check
disconnect/termination key release when the change affects that behavior.
Documentation-only tasks use document, link, and template checks rather than
compiling unrelated Rust code. The optional pre-commit hook documented in the
README runs stricter workspace formatting, Clippy, and test checks when enabled.

Record exact results and limitations. Known existing failures or unavailable
platform checks are not passes. Before marking the PR ready, meet its approved
acceptance criteria, pass required checks, document known limitations, and push
all implementation commits. Keep it draft while required platform verification
is outstanding unless a human explicitly approves a documented exception.

GitHub builds run only when `v*` tags are pushed. Ordinary branch pushes and PRs
do not run GitHub builds, so verify locally. Implementation approval does not
authorize merging, publishing releases, or creating/pushing release tags; those
require separate human authorization.

## Hand off across machines

Push all needed WIP first. On the issue, record:

```text
Handoff: <outgoing agent/session ID> -> <incoming ID, or unclaimed>
Ownership: <released, or explicitly transferred to the incoming agent>
Approved plan: revision <N>, <approval reference>
Draft PR: <URL>
Branch: codex/<issue>-<slug>
Latest pushed commit: <full SHA>
Completed: <implemented outcomes>
Remaining: <next concrete steps>
Checks / limitations: <results, failures, and required platform verification>
Blockers / dependencies: <links, or none>
Local-only material: <none, or explicitly identify what still needs transfer>
```

Do not call a handoff complete until the necessary work is remotely recoverable
or any required artifact transfer is confirmed. Do not put secrets in issue
comments. After release/transfer, the outgoing agent stops writing to the branch.

The receiving agent reads the issue and PR, verifies approval and released or
transferred ownership, then records its own claim. From its primary clone root:

```sh
git fetch origin
git rev-parse origin/codex/42-short-description
git worktree list
git worktree add --track -b codex/42-short-description .worktrees/42-short-description origin/codex/42-short-description
```

Compare the fetched full SHA with the handoff. If it differs, inspect the new
commits and resolve ownership before writing. If the local branch already exists,
inspect it and its tracking status first; add the worktree using that branch
without `-b` only after confirming it matches the handoff and is not checked out
elsewhere. Never reset or force an existing checkout to make the commands work.
Resume with the remote WIP, issue records, and PR rather than requiring the
previous machine's stash or chat history.

## Preserve work before cleanup

Record completion or handoff and release ownership before removing a worktree.
From the primary clone, inspect it first:

```sh
git -C .worktrees/42-short-description status --short --untracked-files=all
git -C .worktrees/42-short-description status --short --ignored
git fetch origin
git -C .worktrees/42-short-description log --oneline origin/codex/42-short-description..HEAD
```

The final command must show no needed local-only commits. Review uncommitted,
untracked, and ignored files; preserve any needed artifacts outside the worktree
before removal. Verify the absolute target is the intended task worktree under
the primary clone's `.worktrees/`, then use `git worktree remove` with that exact
path. Do not force removal, delete files recursively, or remove another agent's
active checkout. Branch deletion is a separate decision after review/integration.
