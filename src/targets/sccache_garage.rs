use anyhow::Result;
use serde_json::Value;

use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use crate::targets::report;

const DEFAULT_PATHS: &[&str] = &["/data/nvme0/garage/meta", "/data/nvme0/garage/data"];

struct SccacheGarageFramework;

impl Framework for SccacheGarageFramework {
    fn name(&self) -> &'static str {
        "sccache-garage"
    }
    fn summary(&self) -> &'static str {
        "Garage-backed sccache object storage (report only)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&StateReport]
    }
}

static FRAMEWORK: SccacheGarageFramework = SccacheGarageFramework;
pub static SCCACHE_GARAGE: &dyn Framework = &FRAMEWORK;

struct StateReport;
impl Variant for StateReport {
    fn name(&self) -> &'static str {
        "state-report"
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
            "Garage/sccache state",
        ))
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(report::report_only_apply(self))
    }
}
