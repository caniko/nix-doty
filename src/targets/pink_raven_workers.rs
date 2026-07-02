use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

const INGEST_SENTINEL: &str = "/var/lib/pink-raven/ingest-running";

struct PinkRavenWorkersFramework;

impl Framework for PinkRavenWorkersFramework {
    fn name(&self) -> &'static str {
        "pink-raven-workers"
    }
    fn summary(&self) -> &'static str {
        "Pink Raven stuck workers and sentinel"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PinkResetStuck, &PinkIngestReport]
    }
}

static FRAMEWORK: PinkRavenWorkersFramework = PinkRavenWorkersFramework;

pub static PINK_RAVEN_WORKERS: &dyn Framework = &FRAMEWORK;

struct PinkResetStuck;
impl Variant for PinkResetStuck {
    fn name(&self) -> &'static str {
        "reset-stuck"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let sentinel_exists = exec::path_exists(INGEST_SENTINEL);
        let failed = exec::run_stdout(&["systemctl", "--failed", "--no-legend", "--plain"])
            .unwrap_or_default();
        let stuck_units: Vec<&str> = failed
            .lines()
            .filter_map(|l| {
                let name = l.split_whitespace().next().unwrap_or("");
                if name.contains("pink-raven") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: INGEST_SENTINEL.into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: stuck_units.len() as u64 + if sentinel_exists { 1 } else { 0 },
            notes: format!(
                "sentinel: {}, stuck units: {}",
                if sentinel_exists { "present" } else { "absent" },
                stuck_units.join(", ") + if stuck_units.is_empty() { "none" } else { "" },
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec![
                    "dry-run: would clear sentinel + reset-failed pink-raven units".into(),
                ],
            });
        }
        if exec::path_exists(INGEST_SENTINEL) {
            exec::remove_file(INGEST_SENTINEL)?;
        }
        exec::run_stdout(&["sudo", "systemctl", "reset-failed"])?;

        let failed = exec::run_stdout(&["systemctl", "--failed", "--no-legend", "--plain"])
            .unwrap_or_default();
        let mut restarted = 0u64;
        for line in failed.lines() {
            let unit = line.split_whitespace().next().unwrap_or("");
            if unit.contains("pink-raven") {
                let _ = exec::run_stdout(&["sudo", "systemctl", "restart", unit]);
                restarted += 1;
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: restarted + 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}

struct PinkIngestReport;
impl Variant for PinkIngestReport {
    fn name(&self) -> &'static str {
        "ingest-report"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::ReportOnly
    }
    fn inspect(&self) -> Result<Inspection> {
        let running = exec::run_stdout(&["systemctl", "is-active", "pink-raven-ingest.service"])
            .unwrap_or_default();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: INGEST_SENTINEL.into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: 0,
            notes: format!("pink-raven-ingest.service: {running}"),
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
