use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

/// Per-user VM state: `~<user>/<stateDir>/<vmPrefix>*` directories, their
/// backup disk images, and `<vm>-home.img` images left beside them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Policy {
    state_dir: String,
    #[serde(default = "default_vm_prefix")]
    vm_prefix: String,
    #[serde(default = "default_backup_match")]
    backup_match: String,
}

fn default_vm_prefix() -> String {
    "vm-".into()
}

fn default_backup_match() -> String {
    "backup".into()
}

impl Policy {
    fn from_settings(settings: &Value) -> Result<Self> {
        let policy = Self::deserialize(settings).context("cluster-vm-state settings")?;
        let dir = &policy.state_dir;
        ensure!(
            !dir.is_empty()
                && !dir.starts_with('/')
                && !dir.contains('\0')
                && !dir.split('/').any(|part| part == ".."),
            "settings.stateDir must be a non-empty path relative to each home"
        );
        ensure!(
            !policy.vm_prefix.is_empty() && !policy.backup_match.is_empty(),
            "settings.vmPrefix and settings.backupMatch must be non-empty"
        );
        Ok(policy)
    }

    fn cluster_dirs(&self) -> Vec<String> {
        exec::all_user_subdirs(&self.state_dir)
    }

    fn display_path(&self) -> String {
        self.cluster_dirs()
            .first()
            .cloned()
            .unwrap_or_else(|| format!("/home/<user>/{}", self.state_dir))
    }

    fn vm_dirs(&self) -> Vec<String> {
        let mut all_vms = Vec::new();
        for cd in self.cluster_dirs() {
            if let Ok(entries) = exec::read_dir(&cd) {
                for e in entries {
                    let name = e.rsplit('/').next().unwrap_or(&e);
                    if name.starts_with(&self.vm_prefix) {
                        all_vms.push(e);
                    }
                }
            }
        }
        all_vms.sort();
        all_vms.dedup();
        all_vms
    }

    fn backup_images(&self, vm_path: &str) -> Vec<(String, u64)> {
        let dir = match std::fs::read_dir(vm_path) {
            Ok(d) => d,
            Err(_) => return vec![],
        };
        dir.filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(&self.backup_match))
            .map(|e| {
                let path = e.path().to_string_lossy().to_string();
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                (path, size)
            })
            .collect()
    }

    fn orphan_home_images(&self, cluster_path: &str) -> Vec<(String, u64)> {
        let dir = match std::fs::read_dir(cluster_path) {
            Ok(d) => d,
            Err(_) => return vec![],
        };
        dir.filter_map(|e| e.ok())
            .filter_map(|e| {
                let file_name = e.file_name().to_string_lossy().to_string();
                let vm_name = file_name.strip_suffix("-home.img")?;
                if !vm_name.starts_with(&self.vm_prefix) {
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

    fn count_backups(&self) -> (u64, u64) {
        let mut count = 0u64;
        let mut bytes = 0u64;
        for vm in self.vm_dirs() {
            for (_, size) in self.backup_images(&vm) {
                count += 1;
                bytes += size;
            }
        }
        (count, bytes)
    }

    fn all_orphan_home_images(&self) -> Vec<(String, u64)> {
        let mut images = Vec::new();
        for cluster in self.cluster_dirs() {
            images.extend(self.orphan_home_images(&cluster));
        }
        images.sort_by(|a, b| a.0.cmp(&b.0));
        images
    }

    fn count_all_vms(&self) -> (u64, u64) {
        let vms = self.vm_dirs();
        let count = vms.len() as u64;
        let bytes: u64 = vms
            .iter()
            .filter_map(|v| exec::total_dir_size(v).ok())
            .sum();
        (count, bytes)
    }
}

enum Kind {
    Backups,
    OrphanHomeImages,
    ClusterVms,
}

impl Variant for Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Backups => "purge-backups",
            Kind::OrphanHomeImages => "purge-orphan-home-images",
            Kind::ClusterVms => "purge-cluster-vms",
        }
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        match self {
            Kind::ClusterVms => Tier::Confirm,
            _ => Tier::Safe,
        }
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &Value::Null)
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let policy = Policy::from_settings(settings)?;
        let (count, bytes, notes) = match self {
            Kind::Backups => {
                let (count, bytes) = policy.count_backups();
                (
                    count,
                    bytes,
                    format!(
                        "backup disk images across all cluster VMs: {count} files, {}",
                        fmt_bytes(bytes)
                    ),
                )
            }
            Kind::OrphanHomeImages => {
                let images = policy.all_orphan_home_images();
                let count = images.len() as u64;
                let bytes = images.iter().map(|(_, size)| *size).sum();
                (
                    count,
                    bytes,
                    format!(
                        "orphan top-level VM home images with matching vm dirs: {count} files, {}",
                        fmt_bytes(bytes)
                    ),
                )
            }
            Kind::ClusterVms => {
                let (count, bytes) = policy.count_all_vms();
                (
                    count,
                    bytes,
                    format!(
                        "all cluster VM directories: {count} dirs, {}",
                        fmt_bytes(bytes)
                    ),
                )
            }
        };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: policy.display_path(),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: count,
            notes,
        })
    }
    fn apply_with_settings(
        &self,
        apply: bool,
        _force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        let policy = Policy::from_settings(settings)?;
        let report = |removed, freed_bytes, skipped, errors| ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes,
            skipped,
            errors,
        };
        let mut removed = 0u64;
        let mut freed = 0u64;
        let mut errors = Vec::new();
        match self {
            Kind::Backups => {
                if !apply {
                    let (count, bytes) = policy.count_backups();
                    return Ok(report(
                        0,
                        0,
                        count,
                        vec![format!(
                            "dry-run: would delete {count} backup images ({})",
                            fmt_bytes(bytes)
                        )],
                    ));
                }
                for vm in policy.vm_dirs() {
                    for (path, size) in policy.backup_images(&vm) {
                        if let Err(e) = exec::remove_file(&path) {
                            errors.push(format!("cannot remove {path}: {e}"));
                        } else {
                            freed += size;
                            removed += 1;
                        }
                    }
                }
            }
            Kind::OrphanHomeImages => {
                let images = policy.all_orphan_home_images();
                if !apply {
                    let bytes = images.iter().map(|(_, size)| *size).sum();
                    return Ok(report(
                        0,
                        0,
                        images.len() as u64,
                        vec![format!(
                            "dry-run: would delete {} orphan VM home images ({})",
                            images.len(),
                            fmt_bytes(bytes)
                        )],
                    ));
                }
                for (path, size) in images {
                    if let Err(e) = exec::remove_file(&path) {
                        errors.push(format!("cannot remove {path}: {e}"));
                    } else {
                        freed += size;
                        removed += 1;
                    }
                }
            }
            Kind::ClusterVms => {
                if !apply {
                    let (count, bytes) = policy.count_all_vms();
                    return Ok(report(
                        0,
                        0,
                        count,
                        vec![format!(
                            "dry-run: would delete {count} VM directories ({})",
                            fmt_bytes(bytes)
                        )],
                    ));
                }
                for vm in policy.vm_dirs() {
                    if let Ok(size) = exec::total_dir_size(&vm) {
                        freed += size;
                    }
                    if let Err(e) = exec::remove_dir_all(&vm) {
                        errors.push(format!("cannot remove {vm}: {e}"));
                        freed = 0;
                    }
                    removed += 1;
                }
            }
        }
        Ok(report(removed, freed, 0, errors))
    }
}

// The name also keeps this target in its legacy slot of reclaim's
// equal-yield tie-break.
struct ClusterVmStateFramework;

impl Framework for ClusterVmStateFramework {
    fn name(&self) -> &'static str {
        "cluster-vm-state"
    }
    fn summary(&self) -> &'static str {
        "Per-user VM disk images and cluster state (settings.stateDir)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&Kind::Backups, &Kind::OrphanHomeImages, &Kind::ClusterVms]
    }
}

static FRAMEWORK: ClusterVmStateFramework = ClusterVmStateFramework;
pub static CLUSTER_VM_STATE: &dyn Framework = &FRAMEWORK;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_requires_relative_state_dir() {
        for bad in [
            serde_json::json!({"stateDir": "x", "vmPrefx": "vm-"}),
            Value::Null,
            serde_json::json!({"stateDir": ""}),
            serde_json::json!({"stateDir": "/abs"}),
            serde_json::json!({"stateDir": "a/../b"}),
            serde_json::json!({"stateDir": "x", "vmPrefix": ""}),
        ] {
            assert!(Policy::from_settings(&bad).is_err(), "{bad}");
        }
        let policy =
            Policy::from_settings(&serde_json::json!({"stateDir": ".local/state/test"})).unwrap();
        assert_eq!(policy.vm_prefix, "vm-");
        assert_eq!(policy.backup_match, "backup");
    }

    #[test]
    fn selection_matches_prefix_and_backup_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let vm = root.join("vm-a");
        std::fs::create_dir(&vm).unwrap();
        std::fs::write(vm.join("disk-backup.qcow2"), "x").unwrap();
        std::fs::write(vm.join("disk.qcow2"), "x").unwrap();
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::write(root.join("vm-a-home.img"), "x").unwrap();
        std::fs::write(root.join("vm-gone-home.img"), "x").unwrap();
        let policy =
            Policy::from_settings(&serde_json::json!({"stateDir": ".local/state/test"})).unwrap();
        let backups = policy.backup_images(vm.to_str().unwrap());
        assert_eq!(backups.len(), 1);
        assert!(backups[0].0.ends_with("disk-backup.qcow2"));
        let orphans = policy.orphan_home_images(root.to_str().unwrap());
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].0.ends_with("vm-a-home.img"));
    }
}
