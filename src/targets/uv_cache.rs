use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

const UV_CACHE_DIR: &str = "/data/nvme0/shared/uv/cache";

struct UvCacheFramework;

impl Framework for UvCacheFramework {
    fn name(&self) -> &'static str {
        "uv-cache"
    }
    fn summary(&self) -> &'static str {
        "Python UV package cache"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&UvCacheClean, &UvCachePruneOlder]
    }
}

static FRAMEWORK: UvCacheFramework = UvCacheFramework;

pub static UV_CACHE: &dyn Framework = &FRAMEWORK;

struct UvCacheClean;
impl Variant for UvCacheClean {
    fn name(&self) -> &'static str {
        "clean"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let size = if exec::path_exists(UV_CACHE_DIR) {
            exec::total_dir_size(UV_CACHE_DIR).ok()
        } else {
            None
        };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: UV_CACHE_DIR.into(),
            size_bytes: size,
            age_oldest_days: None,
            would_remove: 1,
            notes: "runs uv cache clean".into(),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        if dry_run {
            let size = exec::total_dir_size(UV_CACHE_DIR).unwrap_or(0);
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec![format!(
                    "dry-run: would free {:.1} MiB",
                    size as f64 / 1024.0 / 1024.0
                )],
            });
        }
        if exec::path_exists(UV_CACHE_DIR) {
            exec::run_stdout(&["uv", "cache", "clean"])?;
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}

struct UvCachePruneOlder;
impl Variant for UvCachePruneOlder {
    fn name(&self) -> &'static str {
        "prune-older"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Confirm
    }
    fn inspect(&self) -> Result<Inspection> {
        let count = exec::dir_entry_count(UV_CACHE_DIR).unwrap_or(0);
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: UV_CACHE_DIR.into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: count,
            notes: format!("{count} cache entries — removes files untouched >30 days"),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        if dry_run {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec!["dry-run: would prune cache entries older than 30 days".into()],
            });
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
        let dirs = exec::read_dir(UV_CACHE_DIR).unwrap_or_default();
        let mut removed = 0u64;
        let mut freed = 0u64;
        for d in &dirs {
            if let Ok(meta) = std::fs::metadata(d) {
                if let Ok(mtime) = meta.modified() {
                    let mtime_utc: chrono::DateTime<chrono::Utc> = mtime.into();
                    if mtime_utc < cutoff {
                        freed += exec::total_dir_size(d).unwrap_or(0);
                        let _ = exec::remove_dir_all(d);
                        removed += 1;
                    }
                }
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: freed,
            skipped: 0,
            errors: vec![],
        })
    }
}
