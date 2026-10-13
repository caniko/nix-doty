# Changelog

## [Unreleased]

### Changed

- Doty defaults to caller-local user policy under `$XDG_CONFIG_HOME/doty`.
  Existing machine-wide invocations must select `--mode system`. Unmarked legacy
  target documents remain system policy. See `docs/execution-modes.md`.

### Fixed

- Worker argv protection examines complete argument fields, including URI
  query/fragment syntax and percent-encoded paths.
- Proc references preserve literal ` (deleted)` filename suffixes alongside
  the kernel's deleted-object interpretation.
- User inventory and partial metadata analysis retain their own scan budgets
  after non-recursive root/ancestor admission.
- Worktree closeout recognizes ignored bare Git repositories and preserves
  their independent commits before preview or removal.
- Retention protects closed-FD mappings in cache paths containing newlines or
  literal backslash escapes by matching Linux proc pathname spelling.
- Refuse unknown UV settings and relative or dot-component trash paths before
  selecting or modifying cache directories in system mode.
- Preserve ignored nested Git repositories during worktree removal.
- Read-only Nix provenance inspection works without caller HOME admission.
- User timers use systemd-native argument and environment escaping for literal
  spaces, quotes, dollars and percent signs in configured paths.
- Worktree preservation allocations must remain outside the removed checkout.
- Retention recovery instructions preserve execution scope and quoted user roots;
  retirement purge instructions use the positional plan ID accepted by the CLI.
- Trash configuration rejects unknown keys and invalid settings before selecting
  default directories, including in system mode.
- Safe cleanup continues when a skipped Confirm/Risky target has inadmissible
  paths; inspected and executed targets still pass caller-scope admission.
- Worktree removal rejects every preservation artifact inside the checkout,
  including inventories rooted in an ancestor directory, and active Git bisects.
- Reclaim preserves structured provider paths instead of splitting configurable
  pathnames on commas when assigning filesystem surfaces.
- Worktree removal protects closed-descriptor memory mappings with newline and
  mixed literal-escape pathnames in Linux process maps.
- Removal follow-up commands preserve mode and explicit user roots with shell
  quoting; JSON previews contain no trailing human instructions.
- Worktree removal recognizes live-worker path references in path lists and
  structured command arguments, including colon and comma delimiters.
- Recorded removal plans validate their root, plan ID, quarantine directory,
  and deterministic payload paths before preview, recovery, restore, or purge.
- User trash cleanup refuses symlinked `files` and `info` directories before
  inspection or execution while preserving safe unlinking of trashed links.
- `--force` cannot mutate ReportOnly targets. UV inspection and cleanup bind
  the same cache directory; size scans do not traverse symlinked directories.

- Reclaim checks the exact requested available-byte goal independently of the
  usage threshold, stops completed plans early, and exits unsuccessfully for
  failed actions or unmet/unverified goals. Btrfs subvolumes share one goal.
- Unknown yields are notices, not cleanup errors. Live filesystem measurements
  report signed net availability change separately from adapter-reported bytes.

- Model discovery continues past blocked siblings and missing roots; active
  descendant consumers protect entire snapshots. Failed inspections remain
  visible, and incomplete storage measurements carry unknown totals.

- Nix builds provide Git and Linux mount inspection for retention tests and
  pin these tools in the installed runtime wrapper.

### Added

- Shared user/system execution context with caller-owned root admission, scoped
  retention/trash/UV providers, and guarded standalone removal commands.
- Home Manager module with shared target rendering and an optional user timer;
  the NixOS timer explicitly selects system mode.

- Closeout-based `doty worktree remove` preview/apply with verified external
  artifacts, retained branches, worker/protection checks, and optional existing
  Chaosbox allocation links. See `docs/worktree-removal.md`.

- Goal-driven Nix reclaim: retention is applied once (three generations and
  14 days by default), preserving gcroots and direnv roots, followed by bounded
  GC passes. Runtime and logical-byte budgets, streamed progress, bounded log
  tails, and interrupt receipts are documented in `docs/reclaim.md`.
- Reclaim JSON schema 3 represents unknown target estimates as `null`, exposes
  absolute goals and net availability changes, and renders empty plans as JSON.

- Required inspection coverage and `doctor --expected-config` activation-drift
  diagnostics; read-only Nix-build and application-owned service-storage reports.

- Chaosbox scratch-ledger integration for analysis, quarantine and purge, with
  bounded refreshes and explicit release checks that preserve unfinished work.
- Managed-model inventory reconciles download receipts with lifecycle,
  retention and consumer pins; retired models require a shared producer lock
  before removal.
- Persistent retention targets: `cargo-builds` (incremental or whole-target retention), `package-caches`, `model-directories` (explicit retirement and pins), and `retained-state` (approved quarantine with separate purge). Descendant-age, bounded scans, process references and journaled contained removal protect candidates. See `docs/persistent-retention.md`.
- Flake `lib.persistentRetention` capability for downstream target gating.
- `tmp-stale` target framework: purge stale `/tmp/nix-shell.*` and canix-preflight temp directories
- `canix-preflight-cache` target framework: size-cap cleanup for `~/.cache/canix/preflight` project caches
- `purge-orphan-home-images` variant in `chessbender-state` target
- `purge-backups` variant in `steampipe-state` target
- `analyze opencode` read-only scratch analysis (Linux only): bounded top-level `--inventory` plus recursive subtree scans, `--sort oldest` review ordering with an explicit review queue, bounded issue samples with authoritative counts (schema version 2). Never reads contents, follows symlinks, or deletes anything

### Changed

- opencode-cache snapshot inspection uses `total_paths_size_bounded` instead of per-file iteration

- Config module: JSON target config loading with per-variant settings
- `--config` CLI argument to `status` and `run` commands
- `inspect_with_settings` and `apply_with_settings` default methods on `Variant` trait
- `DirScan` bounded directory scanning utilities (total_dir_size_bounded, total_paths_size_bounded)
- Shared report helper (`targets::report`) for report-only path inspection
- 7 new report-only target frameworks: forgejo-runner-cache, foundry-vtt-state, media-stack-state, openvscode-state, pg-backup-state, sccache-garage
- `LlamaPolicy` for configurable pinned files in llama-models target
- `mainProgram` metadata in flake.nix
- Multi-variant support in the NixOS module with assertions
- `ReclaimReport`, `ReclaimFilesystem`, `ReclaimTarget`, `HealthEntry`, `ReclaimTotals`, `TargetScope` types for structured per-filesystem reclaim output
- Support for `Tier::Confirm` in the `run` command — confirm-tier targets are skipped unless `--force` is passed
- `podman-images` target framework with `disk-report` (report-only) and `prune-inactive-older` (confirm tier) variants

### Changed

- `doctor`, `status`, `run` commands now source target configuration from the config module
- All existing targets use bounded directory scanning instead of unbounded total_dir_size
- nix-store-gc variants avoid full /nix/store scan for freed bytes measurement
- NixOS module target entries use flatten + mapAttrsToList for multi-variant generation
- Alphabetized module declarations and re-exports in targets/mod.rs
- `dry_run` parameter renamed to `apply` across all `Variant::apply` methods; logic inverted from `if dry_run` to `if !apply`
- Mount info now resolved via `findmnt --json` for accurate device, fstype, maj_min, and fsroot fields
- Reclaim module rewritten from flat plan list to a structured `ReclaimReport` with per-filesystem target assignment and health entries
- `nh clean` command updated from `--keep-since 14d` to `all -K 14d` syntax
- Reclaim output adapted to new `ReclaimReport` data model (`format_report_human`, `report.filesystems`)

### Fixed

- nix-store-gc errors now explain that freed bytes measurement was skipped
- Proper truncation notes in inspections for oversized directories
