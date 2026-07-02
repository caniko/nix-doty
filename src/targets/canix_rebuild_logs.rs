use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_PATH: &str = "/data/nvme0/can/Projects/canix/logs";
const DEFAULT_MAX_AGE_DAYS: u32 = 14;

struct CanixRebuildLogsFramework;

impl Framework for CanixRebuildLogsFramework {
    fn name(&self) -> &'static str {
        "canix-rebuild-logs"
    }

    fn summary(&self) -> &'static str {
        "Canix rebuild output logs"
    }

    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeOlder]
    }
}

static FRAMEWORK: CanixRebuildLogsFramework = CanixRebuildLogsFramework;

pub static CANIX_REBUILD_LOGS: &dyn Framework = &FRAMEWORK;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default = "default_path")]
    path: PathBuf,
    #[serde(default = "default_max_age_days")]
    max_age_days: u32,
}

fn default_path() -> PathBuf {
    PathBuf::from(DEFAULT_PATH)
}

fn default_max_age_days() -> u32 {
    DEFAULT_MAX_AGE_DAYS
}

impl Settings {
    fn from_value(value: &Value) -> Result<Self> {
        if value.is_null() {
            return Ok(Self {
                path: default_path(),
                max_age_days: default_max_age_days(),
            });
        }
        serde_json::from_value(value.clone()).map_err(Into::into)
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
        self.inspect_with_settings(&Value::Null)
    }

    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let settings = Settings::from_value(settings)?;
        let stale = stale_log_files(&settings.path, settings.max_age_days)?;
        let total_size = stale.iter().filter_map(|f| file_size(f)).sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: settings.path.display().to_string(),
            size_bytes: Some(total_size),
            age_oldest_days: oldest_age_days(&stale),
            would_remove: stale.len() as u64,
            notes: format!(
                "{} rebuild log files older than {} days",
                stale.len(),
                settings.max_age_days
            ),
        })
    }

    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &Value::Null)
    }

    fn apply_with_settings(
        &self,
        apply: bool,
        _force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        let settings = Settings::from_value(settings)?;
        let stale = stale_log_files(&settings.path, settings.max_age_days)?;
        let freed_bytes = stale.iter().filter_map(|f| file_size(f)).sum();
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: stale.len() as u64,
                errors: vec![format!(
                    "dry-run: would remove {} rebuild log files older than {} days",
                    stale.len(),
                    settings.max_age_days
                )],
            });
        }

        let mut removed = 0;
        let mut errors = Vec::new();
        for file in stale {
            match std::fs::remove_file(&file) {
                Ok(()) => removed += 1,
                Err(e) => errors.push(format!("cannot remove {}: {e}", file.display())),
            }
        }
        prune_empty_dirs(&settings.path, &mut errors);

        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes,
            skipped: 0,
            errors,
        })
    }
}

fn stale_log_files(root: &Path, max_age_days: u32) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days as i64);
    let mut files = Vec::new();
    collect_stale_files(root, &cutoff, &mut files)?;
    files.sort();
    Ok(files)
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|metadata| metadata.len()).ok()
}

fn collect_stale_files(
    dir: &Path,
    cutoff: &chrono::DateTime<chrono::Utc>,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_stale_files(&path, cutoff, files)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("txt")
            && metadata
                .modified()
                .map(|mtime| {
                    let mtime_utc: chrono::DateTime<chrono::Utc> = mtime.into();
                    mtime_utc < *cutoff
                })
                .unwrap_or(true)
        {
            files.push(path);
        }
    }
    Ok(())
}

fn oldest_age_days(files: &[PathBuf]) -> Option<u32> {
    let now = chrono::Utc::now();
    files
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
        .filter_map(|meta| meta.modified().ok())
        .map(|mtime| {
            let mtime_utc: chrono::DateTime<chrono::Utc> = mtime.into();
            let days = now.signed_duration_since(mtime_utc).num_days().max(0);
            u32::try_from(days).unwrap_or(u32::MAX)
        })
        .max()
}

fn prune_empty_dirs(root: &Path, errors: &mut Vec<String>) {
    for depth in (0..=2).rev() {
        prune_empty_dirs_at_depth(root, root, depth, errors);
    }
}

fn prune_empty_dirs_at_depth(
    root: &Path,
    dir: &Path,
    target_depth: usize,
    errors: &mut Vec<String>,
) {
    if !dir.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_empty_dirs_at_depth(root, &path, target_depth, errors);
        }
    }
    let depth = dir
        .strip_prefix(root)
        .ok()
        .map(|rel| rel.components().count())
        .unwrap_or(0);
    if depth == target_depth && depth > 0 {
        match std::fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => errors.push(format!("cannot remove empty dir {}: {e}", dir.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn settings_parse_configured_path_and_age() {
        let settings = Settings::from_value(&serde_json::json!({
            "path": "/tmp/canix-logs",
            "maxAgeDays": 21
        }))
        .unwrap();

        assert_eq!(settings.path, PathBuf::from("/tmp/canix-logs"));
        assert_eq!(settings.max_age_days, 21);
    }

    #[test]
    fn purge_older_removes_stale_logs_and_keeps_recent_logs() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs").join("atlas").join("switch");
        fs::create_dir_all(&log_dir).unwrap();
        let stale = log_dir.join("old.txt");
        let recent = log_dir.join("new.txt");
        fs::write(&stale, "old").unwrap();
        fs::write(&recent, "new").unwrap();
        let old = filetime(SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60));
        set_mtime(&stale, old);

        let report = PurgeOlder
            .apply_with_settings(
                true,
                false,
                &serde_json::json!({
                    "path": dir.path().join("logs").display().to_string(),
                    "maxAgeDays": 14
                }),
            )
            .unwrap();

        assert_eq!(report.removed, 1);
        assert!(!stale.exists());
        assert!(recent.exists());
    }

    fn filetime(time: SystemTime) -> fs::FileTimes {
        fs::FileTimes::new().set_modified(time)
    }

    fn set_mtime(path: &Path, time: fs::FileTimes) {
        fs::File::open(path).unwrap().set_times(time).unwrap();
    }
}
