use super::failed_units::list_failed_units;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

// The name also places this target after `nix-store-gc` in reclaim's
// tie-break, so a space goal met by GC skips the reset.
struct PidfileUnitsFramework;

impl Framework for PidfileUnitsFramework {
    fn name(&self) -> &'static str {
        "pidfile-units"
    }
    fn summary(&self) -> &'static str {
        "Failed units matching a pattern and their stale sentinel file"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&ResetMatching]
    }
}

static FRAMEWORK: PidfileUnitsFramework = PidfileUnitsFramework;

pub static PIDFILE_UNITS: &dyn Framework = &FRAMEWORK;

/// Failed units whose name contains `unit_pattern`, with an optional stale
/// sentinel file that blocks them; both come from target settings.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MatchPolicy {
    unit_pattern: String,
    sentinel: Option<String>,
}

impl MatchPolicy {
    fn from_settings(settings: &Value) -> Result<Self> {
        let policy = Self::deserialize(settings).context("pidfile-units settings")?;
        ensure!(
            !policy.unit_pattern.is_empty(),
            "settings.unitPattern must be non-empty"
        );
        ensure!(
            policy
                .sentinel
                .as_deref()
                .is_none_or(|s| s.starts_with('/') && !s.contains('\0')),
            "settings.sentinel must be an absolute path"
        );
        Ok(policy)
    }

    fn matching(&self, units: Vec<String>) -> Vec<String> {
        units
            .into_iter()
            .filter(|u| u.contains(&self.unit_pattern))
            .collect()
    }

    fn inspect(&self, variant: &dyn Variant) -> Inspection {
        let sentinel_exists = self.sentinel.as_deref().is_some_and(exec::path_exists);
        let stuck_units = self.matching(list_failed_units());
        Inspection {
            framework: variant.framework().name(),
            variant: variant.name(),
            path: self
                .sentinel
                .clone()
                .unwrap_or_else(|| "systemctl --failed".into()),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: stuck_units.len() as u64 + u64::from(sentinel_exists),
            notes: format!(
                "sentinel: {}, stuck units: {}",
                match (&self.sentinel, sentinel_exists) {
                    (None, _) => "unset",
                    (Some(_), true) => "present",
                    (Some(_), false) => "absent",
                },
                stuck_units.join(", ") + if stuck_units.is_empty() { "none" } else { "" },
            ),
        }
    }

    /// Order: sentinel delete, global `reset-failed`, re-list, restart matches.
    fn apply(&self, variant: &dyn Variant, apply: bool) -> Result<ApplyReport> {
        let report = |removed, skipped, errors| ApplyReport {
            framework: variant.framework().name(),
            variant: variant.name(),
            removed,
            freed_bytes: 0,
            skipped,
            errors,
        };
        if !apply {
            return Ok(report(
                0,
                1,
                vec![format!(
                    "dry-run: would clear sentinel + reset-failed {} units",
                    self.unit_pattern
                )],
            ));
        }
        if let Some(sentinel) = self.sentinel.as_deref().filter(|s| exec::path_exists(s)) {
            exec::remove_file(sentinel)?;
        }
        exec::run_stdout(&["sudo", "systemctl", "reset-failed"])?;
        let mut restarted = 0u64;
        for unit in self.matching(list_failed_units()) {
            let _ = exec::run_stdout(&["sudo", "systemctl", "restart", &unit]);
            restarted += 1;
        }
        Ok(report(restarted + 1, 0, vec![]))
    }
}

struct ResetMatching;
impl Variant for ResetMatching {
    fn name(&self) -> &'static str {
        "reset-matching"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &Value::Null)
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        Ok(MatchPolicy::from_settings(settings)?.inspect(self))
    }
    fn apply_with_settings(
        &self,
        apply: bool,
        _force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        MatchPolicy::from_settings(settings)?.apply(self, apply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_policy_validates_settings() {
        for bad in [
            Value::Null,
            serde_json::json!({"unitPattern": ""}),
            serde_json::json!({"unitPattern": "x", "sentinel": "relative"}),
            serde_json::json!({"unitPattern": "x", "sentinel": "/a\0b"}),
            serde_json::json!({"unitPattern": "x", "sentinnel": "/run/x"}),
        ] {
            assert!(MatchPolicy::from_settings(&bad).is_err(), "{bad}");
        }
        let policy =
            MatchPolicy::from_settings(&serde_json::json!({"unitPattern": "app-"})).unwrap();
        assert_eq!(
            policy.matching(vec!["app-a.service".into(), "other.service".into()]),
            vec!["app-a.service".to_owned()]
        );
    }
}
