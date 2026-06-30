use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct BtrfsSnapshotsFramework;

impl Framework for BtrfsSnapshotsFramework {
    fn name(&self) -> &'static str {
        "btrfs-snapshots"
    }
    fn summary(&self) -> &'static str {
        "Btrfs snapshot subvolumes (report only — no manager)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&OrphansReport]
    }
}

static FRAMEWORK: BtrfsSnapshotsFramework = BtrfsSnapshotsFramework;

pub static BTRFS_SNAPSHOTS: &dyn Framework = &FRAMEWORK;

const SNAPSHOTS_DIR: &str = "/.snapshots";

struct OrphansReport;
impl Variant for OrphansReport {
    fn name(&self) -> &'static str {
        "orphans-report"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::ReportOnly
    }
    fn inspect(&self) -> Result<Inspection> {
        let subvols = exec::run_stdout(&["btrfs", "subvolume", "list", "-o", SNAPSHOTS_DIR])
            .unwrap_or_default();
        let count = subvols.lines().count() as u64;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: SNAPSHOTS_DIR.into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: count,
            notes: if count > 0 {
                format!("{count} snapshot subvolumes (all orphans — no snapshot manager wired)")
            } else {
                "no snapshot subvolumes found".into()
            },
        })
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 0,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}
