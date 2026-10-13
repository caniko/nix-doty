use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::{Context, Result, ensure};
use serde_json::Value;

fn configured_dirs(settings: &Value) -> Result<Vec<String>> {
    if !settings.is_null() {
        let object = settings
            .as_object()
            .context("trash settings must be an object")?;
        ensure!(
            object.keys().all(|key| key == "paths"),
            "unknown trash setting"
        );
    }
    if let Some(paths) = settings.get("paths") {
        return paths
            .as_array()
            .context("trash paths must be an array")?
            .iter()
            .map(|p| {
                p.as_str()
                    .context("trash path must be a string")
                    .map(str::to_owned)
            })
            .collect();
    }
    Ok(trash_dirs())
}

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
        for entry in exec::read_dir(&files_dir).unwrap_or_default() {
            // Entries are plain files, directories, or symlinks. The old code
            // ran remove_dir_all on everything and counted failures as
            // removed, so plain files were silently kept while reported as
            // freed. Branch on file type and only count successful removals.
            let file_type = match std::fs::symlink_metadata(&entry) {
                Ok(metadata) => metadata.file_type(),
                Err(_) => continue,
            };
            // exec::remove_file refuses symlinks by policy; unlinking a
            // link never follows it, so trash links are unlinked here.
            let size = if file_type.is_dir() {
                exec::total_dir_size(&entry).unwrap_or(0)
            } else if file_type.is_file() {
                exec::file_size(&entry).unwrap_or(0)
            } else {
                0
            };
            let outcome: Result<()> = if file_type.is_dir() {
                exec::remove_dir_all(&entry)
            } else if file_type.is_symlink() {
                std::fs::remove_file(&entry).with_context(|| format!("failed to unlink {entry}"))
            } else {
                exec::remove_file(&entry)
            };
            if outcome.is_ok() {
                removed += 1;
                freed = freed.saturating_add(size);
            }
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
    fn user_scope(&self) -> crate::scope::UserScope {
        crate::scope::UserScope::Trash
    }
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
        self.inspect_with_settings(&Value::Null)
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let dirs = configured_dirs(settings)?;
        let (total_files, total_size) = dirs
            .iter()
            .map(|d| inspect_trash_dir(d))
            .fold((0, 0), |(ac, ab), (c, b)| (ac + c, ab + b));
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: dirs.join(", "),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: total_files,
            notes: format!(
                "{total_files} items across {count} trash dirs",
                count = dirs.len()
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, _force, &Value::Null)
    }
    fn apply_with_settings(
        &self,
        apply: bool,
        _force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        let dirs = configured_dirs(settings)?;
        if !apply {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_settings_never_fall_back_to_discovered_trash() {
        for value in [
            serde_json::json!({"path":["/intended/trash"]}),
            serde_json::json!({"paths":[], "path":["/intended/trash"]}),
            serde_json::json!([]),
            serde_json::json!("/intended/trash"),
            serde_json::json!(false),
        ] {
            assert!(configured_dirs(&value).is_err(), "{value}");
            assert!(TrashEmpty.inspect_with_settings(&value).is_err(), "{value}");
            assert!(
                TrashEmpty.apply_with_settings(true, true, &value).is_err(),
                "{value}"
            );
        }
        // An explicit empty path list must stay empty instead of discovering defaults.
        assert!(
            configured_dirs(&serde_json::json!({"paths":[]}))
                .unwrap()
                .is_empty()
        );
    }

    // doty runs on Linux only; symlinks are part of the fixture.
    #[cfg(unix)]
    #[test]
    fn empty_removes_files_dirs_and_symlinks_and_counts_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let files_dir = dir.path().join("files");
        std::fs::create_dir_all(files_dir.join("subdir")).unwrap();
        std::fs::write(files_dir.join("big.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(files_dir.join("subdir").join("inner.txt"), "inner").unwrap();
        std::os::unix::fs::symlink("big.bin", files_dir.join("linked.bin")).unwrap();

        let (removed, freed) = empty_trash_dir(&dir.path().display().to_string());

        assert!(!files_dir.join("big.bin").exists());
        assert!(!files_dir.join("subdir").exists());
        assert!(std::fs::symlink_metadata(files_dir.join("linked.bin")).is_err());
        assert_eq!(removed, 3);
        assert!(freed >= 4096);
    }
}
