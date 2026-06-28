use anyhow::Result;
use serde::Serialize;

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

pub trait Framework: Sync {
    fn name(&self) -> &'static str;
    fn summary(&self) -> &'static str;
    fn variants(&self) -> &[&'static dyn Variant];
}

pub trait Variant: Sync {
    fn name(&self) -> &'static str;
    fn framework(&self) -> &'static dyn Framework;
    fn tier(&self) -> Tier;
    fn inspect(&self) -> Result<Inspection>;
    fn apply(&self, dry_run: bool, force: bool) -> Result<ApplyReport>;
}

pub struct ApplyCtx {
    pub dry_run: bool,
    pub force: bool,
}
