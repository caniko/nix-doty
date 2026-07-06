use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct ChessbenderStateFramework;

impl Framework for ChessbenderStateFramework {
    fn name(&self) -> &'static str {
        "chessbender-state"
    }
    fn summary(&self) -> &'static str {
        "Chessbender cluster VM disk images and cluster state"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeBackups, &PurgeOrphanHomeImages, &PurgeClusterVms]
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
        .filter(|e| e.file_name().to_string_lossy().contains("backup"))
        .map(|e| {
            let path = e.path().to_string_lossy().to_string();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            (path, size)
        })
        .collect()
}

fn orphan_home_images(cluster_path: &str) -> Vec<(String, u64)> {
    let dir = match std::fs::read_dir(cluster_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    dir.filter_map(|e| e.ok())
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().to_string();
            let vm_name = file_name.strip_suffix("-home.img")?;
            if !vm_name.starts_with("vm-") {
                return None;
            }
            let vm_dir = format!("{cluster_path}/{vm_name}");
            if !exec::path_exists(&vm_dir) {
                return None;
            }
            let path = e.path().to_string_lossy().to_string();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            Some((path, size))
        })
        .collect()
}

struct PurgeBackups;
impl Variant for PurgeBackups {
    fn name(&self) -> &'static str {
        "purge-backups"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let (count, bytes) = count_backups();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: cluster_dirs()
                .first()
                .cloned()
                .unwrap_or_else(|| "/home/<user>/.local/state/chessbender-cluster".into()),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: count,
            notes: format!(
                "backup disk images across all cluster VMs: {count} files, {}",
                fmt_bytes(bytes)
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            let (count, bytes) = count_backups();
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!(
                    "dry-run: would delete {count} backup images ({})",
                    fmt_bytes(bytes)
                )],
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

struct PurgeOrphanHomeImages;
impl Variant for PurgeOrphanHomeImages {
    fn name(&self) -> &'static str {
        "purge-orphan-home-images"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let (count, bytes) = count_orphan_home_images();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: cluster_dirs()
                .first()
                .cloned()
                .unwrap_or_else(|| "/home/<user>/.local/state/chessbender-cluster".into()),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: count,
            notes: format!(
                "orphan top-level VM home images with matching vm dirs: {count} files, {}",
                fmt_bytes(bytes)
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let images = all_orphan_home_images();
        let bytes = images.iter().map(|(_, size)| *size).sum();
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: images.len() as u64,
                errors: vec![format!(
                    "dry-run: would delete {} orphan VM home images ({})",
                    images.len(),
                    fmt_bytes(bytes)
                )],
            });
        }

        let mut removed = 0u64;
        let mut freed = 0u64;
        let mut errors = Vec::new();
        for (path, size) in images {
            if let Err(e) = exec::remove_file(&path) {
                errors.push(format!("cannot remove {path}: {e}"));
            } else {
                freed += size;
                removed += 1;
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
    fn name(&self) -> &'static str {
        "purge-cluster-vms"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Confirm
    }
    fn inspect(&self) -> Result<Inspection> {
        let (count, bytes) = count_all_vms();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: cluster_dirs()
                .first()
                .cloned()
                .unwrap_or_else(|| "/home/<user>/.local/state/chessbender-cluster".into()),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: count,
            notes: format!(
                "all cluster VM directories: {count} dirs, {}",
                fmt_bytes(bytes)
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            let (count, bytes) = count_all_vms();
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!(
                    "dry-run: would delete {count} VM directories ({})",
                    fmt_bytes(bytes)
                )],
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

fn all_orphan_home_images() -> Vec<(String, u64)> {
    let mut images = Vec::new();
    for cluster in cluster_dirs() {
        images.extend(orphan_home_images(&cluster));
    }
    images.sort_by(|a, b| a.0.cmp(&b.0));
    images
}

fn count_orphan_home_images() -> (u64, u64) {
    let images = all_orphan_home_images();
    let count = images.len() as u64;
    let bytes = images.iter().map(|(_, size)| *size).sum();
    (count, bytes)
}

fn count_all_vms() -> (u64, u64) {
    let vms = vm_dirs();
    let count = vms.len() as u64;
    let bytes: u64 = vms
        .iter()
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
