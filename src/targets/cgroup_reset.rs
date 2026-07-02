use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct CgroupResetFramework;

impl Framework for CgroupResetFramework {
    fn name(&self) -> &'static str {
        "cgroup-reset"
    }
    fn summary(&self) -> &'static str {
        "Stuck cgroup scopes and D-state processes"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&StuckScopes]
    }
}

static FRAMEWORK: CgroupResetFramework = CgroupResetFramework;

pub static CGROUP_RESET: &dyn Framework = &FRAMEWORK;

struct StuckScopes;
impl Variant for StuckScopes {
    fn name(&self) -> &'static str {
        "stuck-scopes"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Confirm
    }
    fn inspect(&self) -> Result<Inspection> {
        let d_state =
            exec::run_stdout(&["ps", "-eo", "pid,stat,comm", "--no-headers"]).unwrap_or_default();
        let stuck_count = d_state
            .lines()
            .filter(|l| l.contains("D") || l.contains("T"))
            .count() as u64;

        let abandoned_scopes = exec::run_stdout(&[
            "systemctl",
            "list-units",
            "--state=failed",
            "--no-legend",
            "--plain",
        ])
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(".scope"))
        .count() as u64;

        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/sys/fs/cgroup".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: stuck_count + abandoned_scopes,
            notes: format!(
                "{stuck_count} D/T-state processes, {abandoned_scopes} abandoned scopes"
            ),
        })
    }
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec!["dry-run: would reset stuck scopes".into()],
            });
        }
        if !force {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 0,
                errors: vec!["requires --force to reset stuck scopes".into()],
            });
        }

        exec::run_stdout(&["sudo", "systemctl", "reset-failed"])?;

        let failed_scopes = exec::run_stdout(&[
            "systemctl",
            "list-units",
            "--state=failed",
            "--no-legend",
            "--plain",
        ])
        .unwrap_or_default();
        let mut stopped = 0u64;
        for line in failed_scopes.lines() {
            if let Some(unit) = line.split_whitespace().next() {
                if unit.contains(".scope") {
                    let _ = exec::run_stdout(&["sudo", "systemctl", "stop", unit]);
                    stopped += 1;
                }
            }
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: stopped,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}
