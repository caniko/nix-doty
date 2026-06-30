use anyhow::Result;
use serde_json::Value;

use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use crate::targets::report;

const DEFAULT_PATHS: &[&str] = &[
    "/var/lib/forgejo-runner",
    "/var/lib/containers/cache",
    "/var/lib/containers/storage",
];

struct ForgejoRunnerCacheFramework;

impl Framework for ForgejoRunnerCacheFramework {
    fn name(&self) -> &'static str {
        "forgejo-runner-cache"
    }
    fn summary(&self) -> &'static str {
        "Forgejo runner cache and container state (report only)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&CacheReport]
    }
}

static FRAMEWORK: ForgejoRunnerCacheFramework = ForgejoRunnerCacheFramework;
pub static FORGEJO_RUNNER_CACHE: &dyn Framework = &FRAMEWORK;

struct CacheReport;
impl Variant for CacheReport {
    fn name(&self) -> &'static str {
        "cache-report"
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
            "Forgejo runner cache/state",
        ))
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(report::report_only_apply(self))
    }
}
