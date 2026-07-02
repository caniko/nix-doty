use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct JournaldVacuumFramework;

impl Framework for JournaldVacuumFramework {
    fn name(&self) -> &'static str {
        "journald-vacuum"
    }
    fn summary(&self) -> &'static str {
        "Systemd journal log vacuuming"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&SizeCap, &TimeCap]
    }
}

static FRAMEWORK: JournaldVacuumFramework = JournaldVacuumFramework;

pub static JOURNALD_VACUUM: &dyn Framework = &FRAMEWORK;

struct SizeCap;
impl Variant for SizeCap {
    fn name(&self) -> &'static str {
        "size-cap"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let usage = exec::run_stdout(&["journalctl", "--disk-usage"]).unwrap_or_default();
        let size = usage
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .ok();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/var/log/journal".into(),
            size_bytes: size,
            age_oldest_days: None,
            would_remove: 0,
            notes: format!("journal disk usage: {usage}"),
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
                errors: vec!["dry-run: would run journalctl --vacuum-size=256M".into()],
            });
        }
        let _before = exec::run_stdout(&["journalctl", "--disk-usage"]).ok();
        exec::run_stdout(&["sudo", "journalctl", "--vacuum-size=256M"])?;
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

struct TimeCap;
impl Variant for TimeCap {
    fn name(&self) -> &'static str {
        "time-cap"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/var/log/journal".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: 0,
            notes: "would keep last 14 days of journal".into(),
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
                errors: vec!["dry-run: would run journalctl --vacuum-time=14d".into()],
            });
        }
        exec::run_stdout(&["sudo", "journalctl", "--vacuum-time=14d"])?;
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
