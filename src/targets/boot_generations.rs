use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

struct BootGenerationsFramework;

impl Framework for BootGenerationsFramework {
    fn name(&self) -> &'static str { "boot-generations" }
    fn summary(&self) -> &'static str { "Old bootloader entries and NixOS generations" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&NhLimit]
    }
}

static FRAMEWORK: BootGenerationsFramework = BootGenerationsFramework;

pub static BOOT_GENERATIONS: &dyn Framework = &FRAMEWORK;

struct NhLimit;
impl Variant for NhLimit {
    fn name(&self) -> &'static str { "nh-limit" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::Safe }
    fn inspect(&self) -> Result<Inspection> {
        let boot_entries = exec::read_dir("/boot/loader/entries").ok()
            .map(|files| {
                let count = files.len() as u64;
                let total_size: u64 = files.iter()
                    .filter_map(|f| exec::file_size(f).ok())
                    .sum();
                (count, total_size)
            });
        let (entries, size) = boot_entries.unwrap_or((0, 0));
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/boot/loader/entries".into(),
            size_bytes: Some(size),
            age_oldest_days: None,
            would_remove: entries.saturating_sub(10),
            notes: format!("{entries} boot entries (would keep ~10)"),
        })
    }
    fn apply(&self, dry_run: bool, _force: bool) -> Result<ApplyReport> {
        if dry_run {
            let entries = exec::dir_entry_count("/boot/loader/entries").unwrap_or(0);
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: entries.saturating_sub(10),
                errors: vec!["dry-run: would run nh clean --keep-since 14d to prune boot entries".into()],
            });
        }
        exec::run_stdout(&["nh", "clean", "--keep-since", "14d"])?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}
