use anyhow::Result;
use serde_json::Value;

use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use crate::targets::report;

const DEFAULT_PATHS: &[&str] = &[
    "/data/nvme0/downloads",
    "/data/nvme0/jellyfin",
    "/var/lib/jellyfin",
    "/var/lib/sonarr",
    "/var/lib/radarr",
    "/var/lib/prowlarr",
    "/var/lib/qbittorrent",
    "/var/lib/seerr",
];

struct MediaStackStateFramework;

impl Framework for MediaStackStateFramework {
    fn name(&self) -> &'static str {
        "media-stack-state"
    }
    fn summary(&self) -> &'static str {
        "Media stack state for qBittorrent, *arr, Jellyfin, and Jellyseerr (report only)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&StateReport]
    }
}

static FRAMEWORK: MediaStackStateFramework = MediaStackStateFramework;
pub static MEDIA_STACK_STATE: &dyn Framework = &FRAMEWORK;

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
            "Media stack state",
        ))
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(report::report_only_apply(self))
    }
}
