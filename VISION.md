# Doty vision

Doty ("Do That Yourself") is a NixOS-driven cleanup orchestrator for frameworks
that accumulate temporary local state. It combines a `doty` CLI with a
`services.doty` NixOS module that declares cleanup targets and schedules, so
build caches, trash, snapshots, model stores, and agent scratch space are
cleaned by policy rather than by hand-written `rm` and `nix-collect-garbage`
commands.

## Problem

Long-lived development machines fill up with state whose ownership is not
obvious: Cargo target directories, uv and user caches, Btrfs snapshots, the
freedesktop trash, model repositories, and agent scratch trees. Deleting any of
it while a build or agent is running corrupts work; deleting it naively loses
the evidence an operator needs to explain what happened. Each framework also
has its own retention rules, which ordinary cleanup scripts ignore.

## Principles

- **Plan before apply.** Retention cleanup produces a reviewed plan with
  persisted policy, cutoff, descendant evidence, operator scope, and producer
  lock identities; applying a journal never discovers additional paths.
- **Refuse unsafe traversals.** Symlinked trash roots and unvalidated recorded
  quarantine structures are refused rather than followed.
- **Measure, then decide.** Reclaim works from bounded system storage analysis
  and explicit time or logical-byte goals, and reports incomplete scans,
  unknown exclusive bytes, and potentially partial purges instead of implying
  success.
- **Respect producers.** Incremental cleanup holds Cargo's existing profile
  lock; whole-target, generic cache, and scratch cleanup require an external
  producer-compatible flock anchor.
- **Versioned evidence.** Journals are versioned so an older binary cannot
  bypass newer evidence.
- **Separate user from system work.** User housekeeping and system cleanup are
  distinct modes.
- **Inspection without deletion.** Nix store provenance can be inspected
  through Chaosbox without cleaning anything.

## Scope

- cleanup targets: Btrfs snapshots, freedesktop trash, model inventories, agent
  scratch, retention rules, and the Nix ledger
- `doty reclaim` with mount and free-space goals
- worktree removal from verified closeouts
- quarantine and recovery inventory
- the `services.doty` NixOS module and `home.nix` integration
- `canix host maintenance` forwarding for reclaim options

## Direction

The unreleased changelog continues in the same direction: exact reviewed
retention plans, read-only bounded storage analysis, cooperative purge budgets
with candidate-level goals, and external producer-compatible lock anchors for
broader cleanup.

## Place in the fleet

Canix consumes Doty as a flake input. Atlas's maintenance configuration declares
`services.doty` targets and runs `doty --mode system run` units for cache
pruning, and `canix host maintenance reclaim` forwards reclaim goals to it.
