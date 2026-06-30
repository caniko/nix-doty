use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

fn trash_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    for d in exec::all_user_subdirs(".local/share/Trash") {
        dirs.push(d);
    }
    for vol in ["/data/nvme0", "/data/scratch"] {
        let entry = format!("{vol}/.Trash-1000");
        if exec::path_exists(&entry) {
            dirs.push(entry);
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

struct FreedesktopTrashFramework;

impl Framework for FreedesktopTrashFramework {
    fn name(&self) -> &'static str {
        "freedesktop-trash"
    }
    fn summary(&self) -> &'static str {
        "Freedesktop Trash directories (per-user and volume)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&TrashEmpty]
    }
}

static FRAMEWORK: FreedesktopTrashFramework = FreedesktopTrashFramework;

pub static FREEDESKTOP_TRASH: &dyn Framework = &FRAMEWORK;

fn inspect_trash_dir(td: &str) -> (u64, u64) {
    let files_dir = format!("{td}/files");
    if exec::path_exists(&files_dir) {
        let size = exec::total_dir_size(&files_dir).unwrap_or(0);
        let count = exec::dir_entry_count(&files_dir).unwrap_or(0);
        (count, size)
    } else {
        (0, 0)
    }
}

fn empty_trash_dir(td: &str) -> (u64, u64) {
    let mut removed = 0u64;
    let mut freed = 0u64;
    let files_dir = format!("{td}/files");
    if exec::path_exists(&files_dir) {
        freed += exec::total_dir_size(&files_dir).unwrap_or(0);
        for entry in exec::read_dir(&files_dir).unwrap_or_default() {
            let _ = exec::remove_dir_all(&entry);
            removed += 1;
        }
    }
    let info_dir = format!("{td}/info");
    if exec::path_exists(&info_dir) {
        for entry in exec::read_dir(&info_dir).unwrap_or_default() {
            let _ = exec::remove_file(&entry);
        }
    }
    (removed, freed)
}

struct TrashEmpty;
impl Variant for TrashEmpty {
    fn name(&self) -> &'static str {
        "empty"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Confirm
    }
    fn inspect(&self) -> Result<Inspection> {
        let dirs = trash_dirs();
        let (total_files, total_size) = dirs
            .iter()
            .map(|d| inspect_trash_dir(d))
            .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: format!("{} trash dirs", dirs.len()),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: total_files,
            notes: format!(
                "{total_files} items across {count} trash dirs",
                count = dirs.len()
            ),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        let dirs = trash_dirs();
        if dry_run {
            let (files, bytes) = dirs
                .iter()
                .map(|d| inspect_trash_dir(d))
                .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: files,
                errors: vec![format!(
                    "dry-run: would empty {files} items from {count} trash dirs ({})",
                    fmt_bytes(bytes),
                    count = dirs.len()
                )],
            });
        }
        let mut total_removed = 0u64;
        let mut total_freed = 0u64;
        for d in &dirs {
            let (r, f) = empty_trash_dir(d);
            total_removed += r;
            total_freed += f;
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: total_removed,
            freed_bytes: total_freed,
            skipped: 0,
            errors: vec![],
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
