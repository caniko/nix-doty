use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use serde_json::Value;

const UV_CACHE_DIR: &str = "/data/nvme0/shared/uv/cache";

fn cache_dir(settings: &Value) -> Result<&str> {
    let path = match settings.get("cacheDir") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("cacheDir must be a string"))?,
        None => UV_CACHE_DIR,
    };
    anyhow::ensure!(
        std::path::Path::new(path).is_absolute(),
        "cacheDir must be absolute"
    );
    Ok(path)
}

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
    fn user_scope(&self) -> crate::scope::UserScope {
        crate::scope::UserScope::UvCache
    }
    fn name(&self) -> &'static str {
        "clean"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn scan_paths(&self, settings: &Value) -> Vec<String> {
        cache_dir(settings)
            .map(|path| vec![path.to_owned()])
            .unwrap_or_default()
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let cache = cache_dir(settings)?;
        let size = if exec::path_exists(cache) {
            exec::total_dir_size(cache).ok()
        } else {
            None
        };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: cache.into(),
            size_bytes: size,
            age_oldest_days: None,
            would_remove: 1,
            notes: "runs uv cache clean".into(),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, _force, &Value::Null)
    }
    fn apply_with_settings(
        &self,
        apply: bool,
        _force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        let cache = cache_dir(settings)?;
        if !apply {
            let size = exec::total_dir_size(cache).unwrap_or(0);
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
        let exists = exec::path_exists(cache);
        if exists {
            exec::run_stdout(&["uv", "cache", "clean", "--cache-dir", cache])?;
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: u64::from(exists),
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
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
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
