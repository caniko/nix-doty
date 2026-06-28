use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

const IMMICH_STATE_DIR: &str = "/var/lib/immich";

struct ImmichTemporalFramework;

impl Framework for ImmichTemporalFramework {
    fn name(&self) -> &'static str { "immich-temporal" }
    fn summary(&self) -> &'static str { "Immich temporary and ML cache" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&ImmichTempClear, &ImmichMlCacheReport]
    }
}

static FRAMEWORK: ImmichTemporalFramework = ImmichTemporalFramework;

pub static IMMICH_TEMPORAL: &dyn Framework = &FRAMEWORK;

struct ImmichTempClear;
impl Variant for ImmichTempClear {
    fn name(&self) -> &'static str { "temp-clear" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::Safe }
    fn inspect(&self) -> Result<Inspection> {
        let dirs = ["cache", "tmp", "upload"];
        let mut total_size = 0u64;
        for sub in &dirs {
            let path = format!("{IMMICH_STATE_DIR}/{sub}");
            if exec::path_exists(&path) {
                total_size += exec::total_dir_size(&path).unwrap_or(0);
            }
        }
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: IMMICH_STATE_DIR.into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: total_size,
            notes: format!("{:.1} MiB in temp/cache/upload", total_size as f64 / 1024.0 / 1024.0),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        let dirs = ["cache", "tmp", "upload"];
        let mut removed = 0u64;
        let mut freed = 0u64;
        for sub in &dirs {
            let path = format!("{IMMICH_STATE_DIR}/{sub}");
            if exec::path_exists(&path) {
                if dry_run {
                    freed += exec::total_dir_size(&path).unwrap_or(0);
                    removed += 1;
                } else {
                    freed += exec::total_dir_size(&path).unwrap_or(0);
                    for entry in exec::read_dir(&path).unwrap_or_default() {
                        let _ = if std::fs::metadata(&entry).ok().map(|m| m.is_dir()).unwrap_or(false) {
                            exec::remove_dir_all(&entry)
                        } else {
                            exec::remove_file(&entry)
                        };
                    }
                    removed += 1;
                }
            }
        }
        if dry_run {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: removed,
                errors: vec![format!("dry-run: would free {:.1} MiB", freed as f64 / 1024.0 / 1024.0)],
            });
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed, freed_bytes: freed, skipped: 0, errors: vec![],
        })
    }
}

struct ImmichMlCacheReport;
impl Variant for ImmichMlCacheReport {
    fn name(&self) -> &'static str { "ml-cache-report" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::ReportOnly }
    fn inspect(&self) -> Result<Inspection> {
        let ml_path = format!("{IMMICH_STATE_DIR}/machine-learning");
        let size = if exec::path_exists(&ml_path) {
            exec::total_dir_size(&ml_path).ok()
        } else { None };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: ml_path,
            size_bytes: size,
            age_oldest_days: None,
            would_remove: 0,
            notes: "Immich ML model cache — manual management".into(),
        })
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 0, freed_bytes: 0, skipped: 0, errors: vec![],
        })
    }
}
