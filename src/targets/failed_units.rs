use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct FailedUnitsFramework;

impl Framework for FailedUnitsFramework {
    fn name(&self) -> &'static str {
        "failed-units"
    }
    fn summary(&self) -> &'static str {
        "Systemd units in failed state"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&ResetFailed, &RestartFailed]
    }
}

static FRAMEWORK: FailedUnitsFramework = FailedUnitsFramework;

pub static FAILED_UNITS: &dyn Framework = &FRAMEWORK;

fn list_failed_units() -> Vec<String> {
    exec::run_stdout(&["systemctl", "--failed", "--no-legend", "--plain"])
        .ok()
        .map(|out| {
            out.lines()
                .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

struct ResetFailed;
impl Variant for ResetFailed {
    fn name(&self) -> &'static str {
        "reset-failed"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let failed = list_failed_units();
        let count = failed.len() as u64;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "systemctl --failed".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: count,
            notes: if count > 0 {
                format!("failed units: {}", failed.join(", "))
            } else {
                "no failed units".into()
            },
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let failed = list_failed_units();
        let count = failed.len() as u64;
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!(
                    "dry-run: would reset {count} failed units: {}",
                    failed.join(", ")
                )],
            });
        }
        exec::run_stdout(&["sudo", "systemctl", "reset-failed"])?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: count,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}

struct RestartFailed;
impl Variant for RestartFailed {
    fn name(&self) -> &'static str {
        "restart-failed"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Confirm
    }
    fn inspect(&self) -> Result<Inspection> {
        let failed = list_failed_units();
        let count = failed.len() as u64;
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "systemctl --failed".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: count,
            notes: if count > 0 {
                format!("would restart: {}", failed.join(", "))
            } else {
                "no failed units".into()
            },
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let failed = list_failed_units();
        let count = failed.len() as u64;
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!("dry-run: would restart {count} failed units")],
            });
        }
        exec::run_stdout(&["sudo", "systemctl", "reset-failed"])?;
        for unit in &failed {
            let _ = exec::run_stdout(&["sudo", "systemctl", "restart", unit]);
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: count,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}
