use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

struct ChessbenderStateFramework;

impl Framework for ChessbenderStateFramework {
    fn name(&self) -> &'static str { "chessbender-state" }
    fn summary(&self) -> &'static str { "Chessbender cluster VM disk images and cluster state" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeBackups, &PurgeClusterVms]
    }
}

static FRAMEWORK: ChessbenderStateFramework = ChessbenderStateFramework;

pub static CHESSBENDER_STATE: &dyn Framework = &FRAMEWORK;

fn cluster_dirs() -> Vec<String> {
    exec::all_user_subdirs(".local/state/chessbender-cluster")
}

fn vm_dirs() -> Vec<String> {
    let mut all_vms = Vec::new();
    for cd in cluster_dirs() {
        if let Ok(entries) = exec::read_dir(&cd) {
            for e in entries {
                let name = e.rsplit('/').next().unwrap_or(&e);
                if name.starts_with("vm-") {
                    all_vms.push(e);
                }
            }
        }
    }
    all_vms.sort();
    all_vms.dedup();
    all_vms
}

fn backup_images(vm_path: &str) -> Vec<(String, u64)> {
    let dir = match std::fs::read_dir(vm_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    dir.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name().to_string_lossy().contains("backup")
        })
        .map(|e| {
            let path = e.path().to_string_lossy().to_string();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            (path, size)
        })
        .collect()
}

struct PurgeBackups;
impl Variant for PurgeBackups {
    fn name(&self) -> &'static str { "purge-backups" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::Safe }
    fn inspect(&self) -> Result<Inspection> {
        let (count, bytes) = count_backups();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: cluster_dirs().first().cloned().unwrap_or_else(|| "/home/<user>/.local/state/chessbender-cluster".into()),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: count,
            notes: format!("backup disk images across all cluster VMs: {count} files, {}", fmt_bytes(bytes)),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        if dry_run {
            let (count, bytes) = count_backups();
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!("dry-run: would delete {count} backup images ({})", fmt_bytes(bytes))],
            });
        }
        let mut removed = 0u64;
        let mut freed = 0u64;
        let mut errors = Vec::new();
        for vm in vm_dirs() {
            for (path, size) in backup_images(&vm) {
                if let Err(e) = exec::remove_file(&path) {
                    errors.push(format!("cannot remove {path}: {e}"));
                } else {
                    freed += size;
                    removed += 1;
                }
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: freed,
            skipped: 0,
            errors,
        })
    }
}

struct PurgeClusterVms;
impl Variant for PurgeClusterVms {
    fn name(&self) -> &'static str { "purge-cluster-vms" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::Confirm }
    fn inspect(&self) -> Result<Inspection> {
        let (count, bytes) = count_all_vms();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: cluster_dirs().first().cloned().unwrap_or_else(|| "/home/<user>/.local/state/chessbender-cluster".into()),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: count,
            notes: format!("all cluster VM directories: {count} dirs, {}", fmt_bytes(bytes)),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        if dry_run {
            let (count, bytes) = count_all_vms();
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!("dry-run: would delete {count} VM directories ({})", fmt_bytes(bytes))],
            });
        }
        let mut removed = 0u64;
        let mut freed = 0u64;
        let mut errors = Vec::new();
        for vm in vm_dirs() {
            if let Ok(size) = exec::total_dir_size(&vm) {
                freed += size;
            }
            if let Err(e) = exec::remove_dir_all(&vm) {
                errors.push(format!("cannot remove {vm}: {e}"));
                freed = 0;
            }
            removed += 1;
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: freed,
            skipped: 0,
            errors,
        })
    }
}

fn count_backups() -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    for vm in vm_dirs() {
        for (_, size) in backup_images(&vm) {
            count += 1;
            bytes += size;
        }
    }
    (count, bytes)
}

fn count_all_vms() -> (u64, u64) {
    let vms = vm_dirs();
    let count = vms.len() as u64;
    let bytes: u64 = vms.iter()
        .filter_map(|v| exec::total_dir_size(v).ok())
        .sum();
    (count, bytes)
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
