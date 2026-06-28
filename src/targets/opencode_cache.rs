use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

struct OpencodeCacheFramework;

impl Framework for OpencodeCacheFramework {
    fn name(&self) -> &'static str { "opencode-cache" }
    fn summary(&self) -> &'static str { "Opencode agent session cache: tool output and snapshots (disabled by default)" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeToolOutput, &PurgeSnapshots, &PurgeLogs]
    }
}

static FRAMEWORK: OpencodeCacheFramework = OpencodeCacheFramework;

pub static OPENCODE_CACHE: &dyn Framework = &FRAMEWORK;

fn opencode_dirs() -> Vec<String> {
    exec::all_user_subdirs(".local/share/opencode")
}

fn sum_subdir_bytes(parent: &str, sub: &str) -> (u64, u64) {
    let path = format!("{parent}/{sub}");
    if !exec::path_exists(&path) {
        return (0, 0);
    }
    let entries = exec::read_dir(&path).unwrap_or_default();
    let bytes: u64 = entries.iter()
        .filter_map(|e| exec::total_dir_size(e).ok())
        .sum();
    (entries.len() as u64, bytes)
}

fn remove_subdir(parent: &str, sub: &str) -> Result<(u64, u64, Vec<String>)> {
    let path = format!("{parent}/{sub}");
    if !exec::path_exists(&path) {
        return Ok((0, 0, vec![]));
    }
    let entries = exec::read_dir(&path).unwrap_or_default();
    let bytes: u64 = entries.iter()
        .filter_map(|e| exec::total_dir_size(e).ok())
        .sum();
    let count = entries.len() as u64;
    let mut errors = Vec::new();
    for entry in &entries {
        if let Err(e) = exec::remove_dir_all(entry) {
            errors.push(format!("cannot remove {entry}: {e}"));
        }
    }
    Ok((count, bytes, errors))
}

struct PurgeToolOutput;
impl Variant for PurgeToolOutput {
    fn name(&self) -> &'static str { "purge-tool-output" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::ReportOnly }
    fn inspect(&self) -> Result<Inspection> {
        let dirs = opencode_dirs();
        let (total_count, total_bytes) = dirs.iter()
            .map(|d| sum_subdir_bytes(d, "tool-output"))
            .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/home/<users>/.local/share/opencode/tool-output".into(),
            size_bytes: Some(total_bytes),
            age_oldest_days: None,
            would_remove: total_count,
            notes: format!("opencode tool output cache across {count} users — disabled by default", count = dirs.len()),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        let dirs = opencode_dirs();
        if dry_run {
            let (count, bytes) = dirs.iter()
                .map(|d| sum_subdir_bytes(d, "tool-output"))
                .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: count,
                errors: vec![format!("dry-run: report-only — would delete {count} tool output dirs ({})", fmt_bytes(bytes))],
            });
        }
        let mut total_removed = 0u64;
        let mut total_freed = 0u64;
        let mut errors = Vec::new();
        for d in &dirs {
            let (c, b, e) = remove_subdir(d, "tool-output")?;
            total_removed += c;
            total_freed += b;
            errors.extend(e);
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: total_removed,
            freed_bytes: total_freed,
            skipped: 0,
            errors,
        })
    }
}

struct PurgeSnapshots;
impl Variant for PurgeSnapshots {
    fn name(&self) -> &'static str { "purge-snapshots" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::ReportOnly }
    fn inspect(&self) -> Result<Inspection> {
        let dirs = opencode_dirs();
        let (total_count, total_bytes) = dirs.iter()
            .map(|d| {
                let path = format!("{d}/snapshot");
                if !exec::path_exists(&path) {
                    return (0, 0);
                }
                let entries = exec::read_dir(&path).unwrap_or_default();
                let bytes: u64 = entries.iter()
                    .filter_map(|e| exec::file_size(e).ok())
                    .sum();
                (entries.len() as u64, bytes)
            })
            .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/home/<users>/.local/share/opencode/snapshot".into(),
            size_bytes: Some(total_bytes),
            age_oldest_days: None,
            would_remove: total_count,
            notes: format!("opencode state snapshots across {count} users — disabled by default", count = dirs.len()),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        let dirs = opencode_dirs();
        if dry_run {
            let (count, bytes) = dirs.iter()
                .map(|d| {
                    let path = format!("{d}/snapshot");
                    if !exec::path_exists(&path) {
                        return (0, 0);
                    }
                    let entries = exec::read_dir(&path).unwrap_or_default();
                    let bytes: u64 = entries.iter()
                        .filter_map(|e| exec::file_size(e).ok())
                        .sum();
                    (entries.len() as u64, bytes)
                })
                .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: count,
                errors: vec![format!("dry-run: report-only — would delete {count} snapshot files ({})", fmt_bytes(bytes))],
            });
        }
        let mut total_removed = 0u64;
        let mut total_freed = 0u64;
        let mut errors = Vec::new();
        for d in &dirs {
            let path = format!("{d}/snapshot");
            if !exec::path_exists(&path) {
                continue;
            }
            let entries = exec::read_dir(&path).unwrap_or_default();
            for entry in &entries {
                if let Ok(size) = exec::file_size(entry) {
                    total_freed += size;
                }
                if let Err(e) = exec::remove_file(entry) {
                    errors.push(format!("cannot remove {entry}: {e}"));
                }
                total_removed += 1;
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: total_removed,
            freed_bytes: total_freed,
            skipped: 0,
            errors,
        })
    }
}

struct PurgeLogs;
impl Variant for PurgeLogs {
    fn name(&self) -> &'static str { "purge-logs" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::ReportOnly }
    fn inspect(&self) -> Result<Inspection> {
        let dirs = opencode_dirs();
        let total_bytes: u64 = dirs.iter()
            .map(|d| exec::total_dir_size(&format!("{d}/log")).unwrap_or(0))
            .sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/home/<users>/.local/share/opencode/log".into(),
            size_bytes: Some(total_bytes),
            age_oldest_days: None,
            would_remove: dirs.len() as u64,
            notes: format!("opencode chat logs across {count} users — disabled by default", count = dirs.len()),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        let dirs = opencode_dirs();
        if dry_run {
            let bytes: u64 = dirs.iter()
                .map(|d| exec::total_dir_size(&format!("{d}/log")).unwrap_or(0))
                .sum();
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: dirs.len() as u64,
                errors: vec![format!("dry-run: report-only — would delete opencode logs ({})", fmt_bytes(bytes))],
            });
        }
        let mut total_removed = 0u64;
        let mut total_freed = 0u64;
        let mut errors = Vec::new();
        for d in &dirs {
            let path = format!("{d}/log");
            if !exec::path_exists(&path) { continue; }
            if let Ok(bytes) = exec::total_dir_size(&path) {
                total_freed += bytes;
            }
            match exec::remove_dir_all(&path) {
                Ok(()) => total_removed += 1,
                Err(e) => errors.push(format!("{path}: {e:#}")),
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: total_removed,
            freed_bytes: total_freed,
            skipped: 0,
            errors,
        })
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
