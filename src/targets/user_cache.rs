use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct UserCacheFramework;

impl Framework for UserCacheFramework {
    fn name(&self) -> &'static str {
        "user-cache"
    }
    fn summary(&self) -> &'static str {
        "User-level build caches — removes stale entries older than 30 days across all users"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[
            &PurgeGoBuild,
            &PurgeCabal,
            &PurgeGrype,
            &PurgeComgr,
            &PurgeAppimage,
        ]
    }
}

static FRAMEWORK: UserCacheFramework = UserCacheFramework;

pub static USER_CACHE: &dyn Framework = &FRAMEWORK;

fn stale_cache_inspect(subpath: &str, desc: &str) -> Result<Inspection> {
    let dirs = exec::all_user_subdirs(subpath);
    let scan = exec::total_paths_size_bounded(&dirs, 20_000);
    let path = if dirs.is_empty() {
        format!("/home/<users>/{subpath}")
    } else {
        dirs.join(", ")
    };
    Ok(Inspection {
        framework: "user-cache",
        variant: "",
        path,
        size_bytes: Some(scan.bytes),
        age_oldest_days: None,
        would_remove: dirs.len() as u64,
        notes: format!(
            "{desc} — {count} user dirs with stale entries >30d{suffix}",
            count = dirs.len(),
            suffix = if scan.truncated {
                " (scan truncated)"
            } else {
                ""
            }
        ),
    })
}

fn stale_cache_apply(subpath: &str, apply: bool) -> Result<ApplyReport> {
    let dirs = exec::all_user_subdirs(subpath);
    let before = exec::total_paths_size_bounded(&dirs, 20_000).bytes;
    if !apply {
        return Ok(ApplyReport {
            framework: "",
            variant: "",
            removed: 0,
            freed_bytes: 0,
            skipped: dirs.len() as u64,
            errors: vec![format!(
                "dry-run: would prune stale entries >30d from {count} dirs ({})",
                fmt_bytes(before),
                count = dirs.len()
            )],
        });
    }
    let mut total_removed = 0u64;
    let mut total_freed = 0u64;
    let mut errors = Vec::new();
    for d in &dirs {
        match exec::remove_stale_entries(d, 30, 2) {
            Ok((removed, freed)) => {
                total_removed += removed;
                total_freed += freed;
            }
            Err(e) => errors.push(format!("{d}: {e:#}")),
        }
    }
    Ok(ApplyReport {
        framework: "",
        variant: "",
        removed: total_removed,
        freed_bytes: total_freed,
        skipped: 0,
        errors,
    })
}

struct PurgeGoBuild;
impl Variant for PurgeGoBuild {
    fn name(&self) -> &'static str {
        "purge-go-build"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        stale_cache_inspect(
            ".cache/go-build",
            "Go build cache — stale entries >30d removed",
        )
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let mut r = stale_cache_apply(".cache/go-build", apply)?;
        r.framework = self.framework().name();
        r.variant = self.name();
        Ok(r)
    }
}

struct PurgeCabal;
impl Variant for PurgeCabal {
    fn name(&self) -> &'static str {
        "purge-cabal"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        stale_cache_inspect(
            ".cache/cabal",
            "Cabal Haskell build cache — stale entries >30d removed",
        )
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let mut r = stale_cache_apply(".cache/cabal", apply)?;
        r.framework = self.framework().name();
        r.variant = self.name();
        Ok(r)
    }
}

struct PurgeGrype;
impl Variant for PurgeGrype {
    fn name(&self) -> &'static str {
        "purge-grype"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        stale_cache_inspect(
            ".cache/grype",
            "Grype vulnerability database cache — stale entries >30d removed",
        )
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let mut r = stale_cache_apply(".cache/grype", apply)?;
        r.framework = self.framework().name();
        r.variant = self.name();
        Ok(r)
    }
}

struct PurgeComgr;
impl Variant for PurgeComgr {
    fn name(&self) -> &'static str {
        "purge-comgr"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        stale_cache_inspect(
            ".cache/comgr",
            "AMD ROCm compiler cache — stale entries >30d removed",
        )
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let mut r = stale_cache_apply(".cache/comgr", apply)?;
        r.framework = self.framework().name();
        r.variant = self.name();
        Ok(r)
    }
}

struct PurgeAppimage;
impl Variant for PurgeAppimage {
    fn name(&self) -> &'static str {
        "purge-appimage"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        stale_cache_inspect(
            ".cache/appimage-run",
            "AppImage runner cache — stale entries >30d removed",
        )
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let mut r = stale_cache_apply(".cache/appimage-run", apply)?;
        r.framework = self.framework().name();
        r.variant = self.name();
        Ok(r)
    }
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
