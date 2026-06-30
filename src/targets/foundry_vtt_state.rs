use anyhow::Result;
use serde_json::Value;

use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use crate::targets::report;

const DEFAULT_PATHS: &[&str] = &["/var/lib/foundryvtt"];

struct FoundryVttStateFramework;

impl Framework for FoundryVttStateFramework {
    fn name(&self) -> &'static str {
        "foundry-vtt-state"
    }
    fn summary(&self) -> &'static str {
        "Foundry VTT worlds and state (report only)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&StateReport]
    }
}

static FRAMEWORK: FoundryVttStateFramework = FoundryVttStateFramework;
pub static FOUNDRY_VTT_STATE: &dyn Framework = &FRAMEWORK;

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
            "Foundry VTT state",
        ))
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(report::report_only_apply(self))
    }
}
