# Changelog

## [Unreleased]

### Added

- Config module: JSON target config loading with per-variant settings
- `--config` CLI argument to `status` and `run` commands
- `inspect_with_settings` and `apply_with_settings` default methods on `Variant` trait
- `DirScan` bounded directory scanning utilities (total_dir_size_bounded, total_paths_size_bounded)
- Shared report helper (`targets::report`) for report-only path inspection
- 7 new report-only target frameworks: forgejo-runner-cache, foundry-vtt-state, media-stack-state, openvscode-state, pg-backup-state, sccache-garage
- `LlamaPolicy` for configurable pinned files in llama-models target
- `mainProgram` metadata in flake.nix
- Multi-variant support in the NixOS module with assertions

### Changed

- `doctor`, `status`, `run` commands now source target configuration from the config module
- All existing targets use bounded directory scanning instead of unbounded total_dir_size
- nix-store-gc variants avoid full /nix/store scan for freed bytes measurement
- NixOS module target entries use flatten + mapAttrsToList for multi-variant generation
- Alphabetized module declarations and re-exports in targets/mod.rs

### Fixed

- nix-store-gc errors now explain that freed bytes measurement was skipped
- Proper truncation notes in inspections for oversized directories
