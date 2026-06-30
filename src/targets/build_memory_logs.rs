use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use std::path::Path;

const LOG_DIR: &str = "/var/log/build-memory";

struct BuildMemoryLogsFramework;

impl Framework for BuildMemoryLogsFramework {
    fn name(&self) -> &'static str {
        "build-memory-logs"
    }
    fn summary(&self) -> &'static str {
        "Build memory probe JSONL episode logs"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&RotateByMonth, &PurgeOlder]
    }
}

static FRAMEWORK: BuildMemoryLogsFramework = BuildMemoryLogsFramework;

pub static BUILD_MEMORY_LOGS: &dyn Framework = &FRAMEWORK;

fn list_log_files() -> Vec<String> {
    exec::read_dir(LOG_DIR)
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.ends_with(".jsonl"))
        .collect()
}

struct RotateByMonth;
impl Variant for RotateByMonth {
    fn name(&self) -> &'static str {
        "rotate-by-month"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let files = list_log_files();
        let count = files.len() as u64;
        let total_size: u64 = files.iter().filter_map(|f| exec::file_size(f).ok()).sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: LOG_DIR.into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: count.saturating_sub(3).max(1),
            notes: format!(
                "{count} episode files ({:.1} total)",
                total_size as f64 / 1024.0 / 1024.0
            ),
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
                errors: vec!["dry-run: would keep 3 newest month files, remove rest".into()],
            });
        }
        let mut files = list_log_files();
        files.sort_by(|a, b| {
            std::fs::metadata(b)
                .and_then(|m| m.modified())
                .ok()
                .cmp(&std::fs::metadata(a).and_then(|m| m.modified()).ok())
        });
        let mut removed = 0u64;
        let mut freed = 0u64;
        for f in files.iter().skip(3) {
            if let Ok(sz) = exec::file_size(f) {
                freed += sz;
            }
            exec::remove_file(f)?;
            removed += 1;
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

struct PurgeOlder;
impl Variant for PurgeOlder {
    fn name(&self) -> &'static str {
        "purge-older"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let files = list_log_files();
        let count = files.len() as u64;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: LOG_DIR.into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: count,
            notes: "purges files modified >90 days ago".into(),
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
                errors: vec!["dry-run: would remove files older than 90 days".into()],
            });
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(90);
        let mut removed = 0u64;
        let mut freed = 0u64;
        for f in list_log_files() {
            let path = Path::new(&f);
            if let Ok(meta) = path.metadata() {
                if let Ok(mtime) = meta.modified() {
                    let mtime_utc: chrono::DateTime<chrono::Utc> = mtime.into();
                    if mtime_utc < cutoff {
                        freed += meta.len();
                        exec::remove_file(&f)?;
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
