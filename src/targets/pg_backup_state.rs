use anyhow::Result;
use serde_json::Value;

use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use crate::targets::report;

const DEFAULT_PATHS: &[&str] = &["/var/lib/postgresql", "/var/lib/pg-backup"];

struct PgBackupStateFramework;

impl Framework for PgBackupStateFramework {
    fn name(&self) -> &'static str {
        "pg-backup-state"
    }
    fn summary(&self) -> &'static str {
        "PostgreSQL backup and WAL state (report only)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&BackupReport]
    }
}

static FRAMEWORK: PgBackupStateFramework = PgBackupStateFramework;
pub static PG_BACKUP_STATE: &dyn Framework = &FRAMEWORK;

struct BackupReport;
impl Variant for BackupReport {
    fn name(&self) -> &'static str {
        "backup-report"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::ReportOnly
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Object(Default::default()))
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        Ok(report::inspect_paths(
            self.framework().name(),
            self.name(),
            DEFAULT_PATHS,
            settings,
            "PostgreSQL backup/WAL state",
        ))
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(report::report_only_apply(self))
    }
}
