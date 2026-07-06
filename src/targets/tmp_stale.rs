use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const TMP_DIR: &str = "/tmp";
const DEFAULT_MAX_AGE_DAYS: i64 = 1;

struct TmpStaleFramework;

impl Framework for TmpStaleFramework {
    fn name(&self) -> &'static str {
        "tmp-stale"
    }

    fn summary(&self) -> &'static str {
        "Stale top-level /tmp build and shell directories"
    }

    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeNixShellDirs, &PurgeCanixPreflightTemp]
    }
}

static FRAMEWORK: TmpStaleFramework = TmpStaleFramework;

pub static TMP_STALE: &dyn Framework = &FRAMEWORK;

struct PurgeNixShellDirs;

impl Variant for PurgeNixShellDirs {
    fn name(&self) -> &'static str {
        "purge-nix-shell-dirs"
    }

    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }

    fn tier(&self) -> Tier {
        Tier::Safe
    }

    fn inspect(&self) -> Result<Inspection> {
        let candidates = stale_candidates(&[NameMatch::Prefix("nix-shell.")])?;
        Ok(inspection(
            self,
            candidates,
            "stale /tmp/nix-shell.* directories",
        ))
    }

    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        apply_candidates(
            self,
            stale_candidates(&[NameMatch::Prefix("nix-shell.")])?,
            apply,
        )
    }
}

struct PurgeCanixPreflightTemp;

impl Variant for PurgeCanixPreflightTemp {
    fn name(&self) -> &'static str {
        "purge-canix-preflight-temp"
    }

    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }

    fn tier(&self) -> Tier {
        Tier::Safe
    }

    fn inspect(&self) -> Result<Inspection> {
        let candidates = stale_candidates(&[
            NameMatch::Prefix("canix-preflight-"),
            NameMatch::Exact("canix-cargo-check"),
        ])?;
        Ok(inspection(
            self,
            candidates,
            "stale canix preflight /tmp directories",
        ))
    }

    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        apply_candidates(
            self,
            stale_candidates(&[
                NameMatch::Prefix("canix-preflight-"),
                NameMatch::Exact("canix-cargo-check"),
            ])?,
            apply,
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum NameMatch {
    Prefix(&'static str),
    Exact(&'static str),
}

impl NameMatch {
    fn matches(self, name: &str) -> bool {
        match self {
            Self::Prefix(prefix) => name.starts_with(prefix),
            Self::Exact(exact) => name == exact,
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    bytes: u64,
    age_days: u32,
}

fn stale_candidates(patterns: &[NameMatch]) -> Result<Vec<Candidate>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(DEFAULT_MAX_AGE_DAYS);
    let mut candidates = Vec::new();

    for entry in fs::read_dir(TMP_DIR).context("failed to read /tmp")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !patterns.iter().any(|pattern| pattern.matches(&name)) {
            continue;
        }

        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_dir() {
            continue;
        }

        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let modified_utc: chrono::DateTime<chrono::Utc> = modified.into();
        if modified_utc >= cutoff {
            continue;
        }

        candidates.push(Candidate {
            bytes: total_size_no_follow(&path).unwrap_or(0),
            age_days: age_days(modified),
            path,
        });
    }

    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(candidates)
}

fn inspection(variant: &dyn Variant, candidates: Vec<Candidate>, label: &str) -> Inspection {
    let bytes = candidates.iter().map(|candidate| candidate.bytes).sum();
    let oldest = candidates.iter().map(|candidate| candidate.age_days).max();
    let count = candidates.len() as u64;
    Inspection {
        framework: variant.framework().name(),
        variant: variant.name(),
        path: TMP_DIR.into(),
        size_bytes: Some(bytes),
        age_oldest_days: oldest,
        would_remove: count,
        notes: format!(
            "{label}: {count} directories older than {DEFAULT_MAX_AGE_DAYS} day, {}",
            fmt_bytes(bytes)
        ),
    }
}

fn apply_candidates(
    variant: &dyn Variant,
    candidates: Vec<Candidate>,
    apply: bool,
) -> Result<ApplyReport> {
    let count = candidates.len() as u64;
    let bytes = candidates.iter().map(|candidate| candidate.bytes).sum();
    if !apply {
        return Ok(ApplyReport {
            framework: variant.framework().name(),
            variant: variant.name(),
            removed: 0,
            freed_bytes: 0,
            skipped: count,
            errors: vec![format!(
                "dry-run: would remove {count} stale /tmp directories ({})",
                fmt_bytes(bytes)
            )],
        });
    }

    let mut removed = 0;
    let mut freed_bytes: u64 = 0;
    let mut errors = Vec::new();
    for candidate in candidates {
        match fs::remove_dir_all(&candidate.path) {
            Ok(()) => {
                removed += 1;
                freed_bytes = freed_bytes.saturating_add(candidate.bytes);
            }
            Err(e) => errors.push(format!("cannot remove {}: {e}", candidate.path.display())),
        }
    }

    Ok(ApplyReport {
        framework: variant.framework().name(),
        variant: variant.name(),
        removed,
        freed_bytes,
        skipped: 0,
        errors,
    })
}

fn total_size_no_follow(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total = total.saturating_add(total_size_no_follow(&entry?.path())?);
    }
    Ok(total)
}

fn age_days(modified: SystemTime) -> u32 {
    let modified_utc: chrono::DateTime<chrono::Utc> = modified.into();
    let days = chrono::Utc::now()
        .signed_duration_since(modified_utc)
        .num_days()
        .max(0);
    u32::try_from(days).unwrap_or(u32::MAX)
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}
