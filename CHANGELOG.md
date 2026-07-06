# Changelog

## [Unreleased]

### Added

- `tmp-stale` target framework: purge stale `/tmp/nix-shell.*` and canix-preflight temp directories
- `canix-preflight-cache` target framework: size-cap cleanup for `~/.cache/canix/preflight` project caches
- `purge-orphan-home-images` variant in `chessbender-state` target
- `purge-backups` variant in `steampipe-state` target

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
