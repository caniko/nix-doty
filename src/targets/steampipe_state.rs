use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

const VM_SCAN_MAX_ENTRIES: u64 = 20_000;

struct SteampipeStateFramework;

impl Framework for SteampipeStateFramework {
    fn name(&self) -> &'static str {
        "steampipe-state"
    }
    fn summary(&self) -> &'static str {
        "Steampipe AI training VM disk images and cached state"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeStaleVms, &PurgeAllVms]
    }
}

static FRAMEWORK: SteampipeStateFramework = SteampipeStateFramework;

pub static STEAMPIPE_STATE: &dyn Framework = &FRAMEWORK;

fn steampipe_dirs() -> Vec<String> {
    exec::all_user_subdirs(".local/state/steampipe")
}

struct PurgeStaleVms;
impl Variant for PurgeStaleVms {
    fn name(&self) -> &'static str {
        "purge-stale-vms"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let (stale_entries, stale_bytes, notes) = find_stale_vms()?;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: steampipe_dirs()
                .first()
                .cloned()
                .unwrap_or_else(|| "/home/<user>/.local/state/steampipe".into()),
            size_bytes: Some(stale_bytes),
            age_oldest_days: None,
            would_remove: stale_entries,
            notes,
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            let (entries, bytes, _) = find_stale_vms()?;
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: entries,
                errors: vec![format!(
                    "dry-run: would delete {entries} stale VM images ({})",
                    fmt_bytes(bytes)
                )],
            });
        }
        let mut removed = 0u64;
        let mut freed = 0u64;
        let mut errors = Vec::new();
        for dir in steampipe_dirs() {
            for scenario_path in child_dirs(&dir) {
                for entry in child_dirs(&scenario_path) {
                    let name = entry.rsplit('/').next().unwrap_or(&entry);
                    if !is_vm_dir(name) {
                        continue;
                    }
                    let pid_path = format!("{scenario_path}/{name}.pid");
                    let pid = read_pid_if_exists(&pid_path);
                    if let Some(p) = pid {
                        if !is_pid_stale(p) {
                            continue;
                        }
                    }
                    if let Ok(size) = exec::total_dir_size(&entry) {
                        freed += size;
                    }
                    if let Err(e) = exec::remove_dir_all(&entry) {
                        errors.push(format!("cannot remove {entry}: {e}"));
                    }
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

struct PurgeAllVms;
impl Variant for PurgeAllVms {
    fn name(&self) -> &'static str {
        "purge-all-vms"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Risky
    }
    fn inspect(&self) -> Result<Inspection> {
        let (entries, bytes, notes) = calc_all_vm_sizes()?;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: steampipe_dirs()
                .first()
                .cloned()
                .unwrap_or_else(|| "/home/<user>/.local/state/steampipe".into()),
            size_bytes: Some(bytes),
            age_oldest_days: None,
            would_remove: entries,
            notes,
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            let (entries, bytes, _) = calc_all_vm_sizes()?;
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: entries,
                errors: vec![format!(
                    "dry-run: would delete all {entries} VM images ({})",
                    fmt_bytes(bytes)
                )],
            });
        }
        let (_, bytes, _) = calc_all_vm_sizes()?;
        let mut removed = 0u64;
        let mut errors = Vec::new();
        for dir in steampipe_dirs() {
            for scenario_path in child_dirs(&dir) {
                for entry in child_dirs(&scenario_path) {
                    let name = entry.rsplit('/').next().unwrap_or(&entry);
                    if !is_vm_dir(name) {
                        continue;
                    }
                    if let Err(e) = exec::remove_dir_all(&entry) {
                        errors.push(format!("cannot remove {entry}: {e}"));
                    }
                    removed += 1;
                }
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: bytes,
            skipped: 0,
            errors,
        })
    }
}

fn is_vm_dir(name: &str) -> bool {
    name.starts_with("vm-") || name == "fixture"
}

fn child_dirs(path: &str) -> Vec<String> {
    exec::read_dir(path)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| {
            std::fs::metadata(entry)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
        })
        .collect()
}

fn read_pid_if_exists(path: &str) -> Option<u32> {
    exec::read_file(path).ok()?.trim().parse::<u32>().ok()
}

fn is_pid_stale(pid: u32) -> bool {
    let out = exec::run_stdout(&["kill", "-0", &pid.to_string()]);
    out.is_err()
}

fn find_stale_vms() -> Result<(u64, u64, String)> {
    let mut total_entries = 0u64;
    let mut total_bytes = 0u64;

    for dir in steampipe_dirs() {
        for scenario_path in child_dirs(&dir) {
            for entry in child_dirs(&scenario_path) {
                let name = entry.rsplit('/').next().unwrap_or(&entry);
                if !is_vm_dir(name) {
                    continue;
                }
                let pid_path = format!("{scenario_path}/{name}.pid");
                let pid = read_pid_if_exists(&pid_path);
                if let Some(p) = pid {
                    if !is_pid_stale(p) {
                        continue;
                    }
                }
                if let Ok(s) = exec::total_dir_size_bounded(&entry, VM_SCAN_MAX_ENTRIES) {
                    total_bytes += s.bytes;
                }
                total_entries += 1;
            }
        }
    }

    let notes = format!(
        "stale steampipe VM disk images: {total_entries} dirs, {}",
        fmt_bytes(total_bytes)
    );
    Ok((total_entries, total_bytes, notes))
}

fn calc_all_vm_sizes() -> Result<(u64, u64, String)> {
    let mut total_entries = 0u64;
    let mut total_bytes = 0u64;

    for dir in steampipe_dirs() {
        for scenario_path in child_dirs(&dir) {
            for entry in child_dirs(&scenario_path) {
                let name = entry.rsplit('/').next().unwrap_or(&entry);
                if !is_vm_dir(name) {
                    continue;
                }
                if let Ok(s) = exec::total_dir_size_bounded(&entry, VM_SCAN_MAX_ENTRIES) {
                    total_bytes += s.bytes;
                }
                total_entries += 1;
            }
        }
    }

    let notes = format!(
        "ALL steampipe VM disk images: {total_entries} dirs, {}",
        fmt_bytes(total_bytes)
    );
    Ok((total_entries, total_bytes, notes))
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
