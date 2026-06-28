use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

const TRASH_VOLUMES: &[&str] = &["/data/nvme0/.Trash-1000", "/data/scratch/.Trash-1000"];

struct FreedesktopTrashFramework;

impl Framework for FreedesktopTrashFramework {
    fn name(&self) -> &'static str { "freedesktop-trash" }
    fn summary(&self) -> &'static str { "Freedesktop Trash directories" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&TrashEmpty]
    }
}

static FRAMEWORK: FreedesktopTrashFramework = FreedesktopTrashFramework;

pub static FREEDESKTOP_TRASH: &dyn Framework = &FRAMEWORK;

struct TrashEmpty;
impl Variant for TrashEmpty {
    fn name(&self) -> &'static str { "empty" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::Confirm }
    fn inspect(&self) -> Result<Inspection> {
        let mut total_size = 0u64;
        let mut total_files = 0u64;
        for vol in TRASH_VOLUMES {
            let files_dir = format!("{vol}/files");
            if exec::path_exists(&files_dir) {
                total_size += exec::total_dir_size(&files_dir).unwrap_or(0);
                total_files += exec::dir_entry_count(&files_dir).unwrap_or(0);
            }
        }
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "Freedesktop Trash".into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: total_files,
            notes: format!("{total_files} items across {} volumes", TRASH_VOLUMES.len()),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        if dry_run {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: 1,
                errors: vec!["dry-run: would empty trash on all volumes".into()],
            });
        }
        let mut total_freed = 0u64;
        let mut removed = 0u64;
        for vol in TRASH_VOLUMES {
            let files_dir = format!("{vol}/files");
            if exec::path_exists(&files_dir) {
                total_freed += exec::total_dir_size(&files_dir).unwrap_or(0);
                for entry in exec::read_dir(&files_dir).unwrap_or_default() {
                    let _ = exec::remove_dir_all(&entry);
                    removed += 1;
                }
            }
            let info_dir = format!("{vol}/info");
            if exec::path_exists(&info_dir) {
                for entry in exec::read_dir(&info_dir).unwrap_or_default() {
                    let _ = exec::remove_file(&entry);
                }
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: total_freed,
            skipped: 0,
            errors: vec![],
        })
    }
}
