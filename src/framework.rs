use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Safe,
    Confirm,
    Risky,
    ReportOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub framework: &'static str,
    pub variant: &'static str,
    pub path: String,
    pub size_bytes: Option<u64>,
    pub age_oldest_days: Option<u32>,
    pub would_remove: u64,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub framework: &'static str,
    pub variant: &'static str,
    pub removed: u64,
    pub freed_bytes: u64,
    pub skipped: u64,
    pub errors: Vec<String>,
}

/// Reclaim distinguishes an unmeasured yield from a measured zero.
pub struct ReclaimResult {
    pub freed_bytes: Option<u64>,
    pub notices: Vec<String>,
    pub errors: Vec<String>,
    pub command_log: String,
}

impl From<ApplyReport> for ReclaimResult {
    fn from(report: ApplyReport) -> Self {
        Self {
            freed_bytes: Some(report.freed_bytes),
            notices: Vec::new(),
            errors: report.errors,
            command_log: String::new(),
        }
    }
}

pub trait Framework: Sync {
    fn name(&self) -> &'static str;
    fn summary(&self) -> &'static str;
    fn variants(&self) -> &[&'static dyn Variant];
}

pub trait Variant: Sync {
    fn name(&self) -> &'static str;
    fn framework(&self) -> &'static dyn Framework;
    fn tier(&self) -> Tier;
    /// Providers opt in only once all their execution paths honor local settings.
    fn user_scope(&self) -> crate::scope::UserScope {
        crate::scope::UserScope::SystemOnly
    }
    fn inspect(&self) -> Result<Inspection>;
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport>;
    /// Pure scan-surface discovery for doctor, including provider-owned defaults.
    fn scan_paths(&self, settings: &Value) -> Vec<String> {
        let mut paths: Vec<String> = settings["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if let Some(path) = settings["path"].as_str() {
            paths.push(path.to_owned());
        }
        paths
    }
    fn inspect_with_settings(&self, _settings: &Value) -> Result<Inspection> {
        self.inspect()
    }
    fn apply_with_settings(
        &self,
        apply: bool,
        force: bool,
        _settings: &Value,
    ) -> Result<ApplyReport> {
        self.apply(apply, force)
    }
    fn apply_scoped(
        &self,
        apply: bool,
        force: bool,
        settings: &Value,
        context: &crate::scope::ExecutionContext,
    ) -> Result<ApplyReport> {
        let settings = context.settings(self.user_scope(), settings)?;
        self.apply_with_settings(apply, force, &settings)
    }
    fn reclaim_scoped(
        &self,
        force: bool,
        settings: &Value,
        action: &crate::reclaim::runtime::ActionContext<'_>,
        context: &crate::scope::ExecutionContext,
    ) -> Result<ReclaimResult> {
        if context.mode == crate::scope::Mode::System {
            self.reclaim_with_settings(force, settings, action)
        } else {
            crate::reclaim::runtime::with_deadline(action.runtime.deadline, || {
                self.apply_scoped(true, force, settings, context)
                    .map(Into::into)
            })
        }
    }
    fn reclaim_with_settings(
        &self,
        force: bool,
        settings: &Value,
        context: &crate::reclaim::runtime::ActionContext<'_>,
    ) -> Result<ReclaimResult> {
        crate::reclaim::runtime::with_deadline(context.runtime.deadline, || {
            self.apply_with_settings(true, force, settings)
                .map(Into::into)
        })
    }
}

pub struct ApplyCtx {
    pub dry_run: bool,
    pub force: bool,
}
